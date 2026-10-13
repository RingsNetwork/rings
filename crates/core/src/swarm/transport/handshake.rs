//! The connection handshake: offers, answers, and crossing offers.
//!
//! An offer reserves a pending generation whose signaling state ([`Handshake`]) records who
//! drives it and how far it has got; the state lives and dies with the pending record. Two
//! crossing offers are decided by `Law (Crossing offer)` (see `pending::handshake`): an offer
//! still being created yields, and of two sent overlay offers the smaller DID's prevails and is
//! sent back to the peer as a counter-offer. Over the DHT, offers record their path and every
//! reply retraces it (#947, see the `signaling` module), so an edge both ends want is
//! established whenever either overlay offer is delivered.
//!
//! A WebRTC step of an attempt that lost its slot reports why ([`classify_step_failure`]):
//! `ConnectionAttemptSuperseded` when the peer's own handshake took the slot, and
//! `ConnectionAttemptCancelled` when anything else released it. On native WebRTC the first is
//! what surfaced as `Local connection closed` from an offer whose ICE gathering the tie-break
//! interrupted (#945).
//!
//! [`classify_step_failure`]: SwarmTransport::classify_step_failure

use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;

use super::pending::crossing_offer_verdict;
use super::pending::AnswerSlot;
use super::pending::CrossingOfferVerdict;
use super::pending::Handshake;
use super::pending::OfferOwner;
use super::pending::PendingConnectionAttempt;
use super::pending::RawConnectionOwner;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::swarm::callback::InnerSwarmCallback;

/// The DHT signaling of the handshake: offers and replies over recorded paths (#947).
mod signaling;

/// What a node replies to a received connection offer: an answer, or its own prevailing offer
/// (`Law (Tie-break)`), in whatever form the caller carries them.
#[derive(Clone, Debug)]
pub enum OfferReply<A, O> {
    /// The answer to the offer.
    Answer(A),
    /// The replying node's own unanswered offer to the same peer, which prevails on the
    /// tie-break: the peer abandons its offer and answers this one.
    CounterOffer(O),
}

impl<A, O> OfferReply<A, O> {
    /// Map the answer and the counter-offer independently.
    pub fn map<A2, O2>(
        self,
        answer: impl FnOnce(A) -> A2,
        counter_offer: impl FnOnce(O) -> O2,
    ) -> OfferReply<A2, O2> {
        match self {
            Self::Answer(reply) => OfferReply::Answer(answer(reply)),
            Self::CounterOffer(reply) => OfferReply::CounterOffer(counter_offer(reply)),
        }
    }
}

impl<A, O> OfferReply<A, O> {
    /// The answer, or `None` for a counter-offer.
    pub fn answer(self) -> Option<A> {
        match self {
            Self::Answer(answer) => Some(answer),
            Self::CounterOffer(_) => None,
        }
    }
}

impl<T> OfferReply<T, T> {
    /// The reply in its common carried form, whichever it is.
    pub fn into_inner(self) -> T {
        match self {
            Self::Answer(reply) | Self::CounterOffer(reply) => reply,
        }
    }
}

/// An offer reply as the two signaling messages.
pub(crate) type SignalingReply = OfferReply<ConnectNodeReport, ConnectNodeSend>;

impl From<SignalingReply> for Message {
    fn from(reply: SignalingReply) -> Self {
        match reply {
            OfferReply::Answer(answer) => Message::ConnectNodeReport(answer),
            OfferReply::CounterOffer(offer) => Message::ConnectNodeSend(offer),
        }
    }
}

/// An answer this node created, with the generation reserved for it.
#[derive(Clone, Debug)]
pub(crate) struct CreatedAnswer {
    /// The answering generation.
    pub(crate) attempt: PendingConnectionAttempt,
    /// The answer.
    pub(crate) report: ConnectNodeReport,
}

/// This node's own sent offer, sent back to a peer whose crossing offer it prevails over.
#[derive(Clone, Debug)]
pub(crate) struct CounterOffer {
    /// The offer as first sent.
    pub(crate) offer: ConnectNodeSend,
    /// The transaction the offer first left under; the answer to the counter-offer echoes it.
    pub(crate) tx_id: uuid::Uuid,
}

/// What this node does with a received offer.
pub(crate) type OfferOutcome = OfferReply<CreatedAnswer, CounterOffer>;

/// The admitted record an incoming offer meets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IncomingOfferAdmittedPeer {
    /// No admitted record: the offer may proceed to the unadmitted checks.
    Vacant,
    /// An admitted, routable connection: the offer is refused.
    Routable,
    /// An admitted generation that can no longer route; it is retired before the offer
    /// proceeds, unless retirement keeps it.
    Unroutable(PendingConnectionAttempt),
}

impl SwarmTransport {
    /// Classify the admitted record, if any, an offer from `peer` meets.
    fn incoming_offer_admitted_peer(&self, peer: Did) -> Result<IncomingOfferAdmittedPeer> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Ok(IncomingOfferAdmittedPeer::Vacant);
            };
            if self.peer_lifecycles()?.sendable_attempt(peer) != Some(attempt) {
                return Ok(IncomingOfferAdmittedPeer::Unroutable(attempt));
            }
            let Some(connection) = self.get_raw_connection(peer) else {
                return Ok(IncomingOfferAdmittedPeer::Unroutable(attempt));
            };
            if connection.readiness().can_make_progress() {
                return Ok(IncomingOfferAdmittedPeer::Routable);
            }
            Ok(IncomingOfferAdmittedPeer::Unroutable(attempt))
        })
    }

    /// Why `attempt` no longer owns its slot: `ConnectionAttemptSuperseded` when the peer's own
    /// handshake took it, `ConnectionAttemptCancelled` when anything else released it.
    fn lost_slot(&self, attempt: PendingConnectionAttempt) -> Result<Error> {
        let superseded = self.with_connection_lifecycle(|| {
            Ok(self.peer_lifecycles()?.superseded_by_peer(attempt))
        })?;
        Ok(if superseded {
            Error::ConnectionAttemptSuperseded {
                peer: attempt.peer,
                generation: attempt.generation,
            }
        } else {
            Error::ConnectionAttemptCancelled {
                peer: attempt.peer,
                generation: attempt.generation,
            }
        })
    }

    /// The error a failed step of `attempt` reports. An attempt that lost its slot meanwhile
    /// reports why ([`Self::lost_slot`]); a current one is abandoned and reports `failure`.
    /// A poisoned lifecycle lock is propagated.
    async fn classify_step_failure(
        &self,
        attempt: PendingConnectionAttempt,
        failure: Error,
        operation: &str,
    ) -> Result<Error> {
        if !self.is_current_connection_attempt(attempt)? {
            return self.lost_slot(attempt);
        }
        self.abandon_pending_connection(attempt, operation).await;
        Ok(failure)
    }

    /// Apply `Creating -> Sent(offer, tx_id)` to `attempt`; when it no longer owns its slot,
    /// the error says why ([`Self::lost_slot`]).
    pub(in crate::swarm) fn mark_offer_sent(
        &self,
        attempt: PendingConnectionAttempt,
        offer: &ConnectNodeSend,
        tx_id: uuid::Uuid,
    ) -> Result<()> {
        let marked = self.with_connection_lifecycle(|| {
            Ok(self
                .peer_lifecycles()?
                .mark_offer_sent(attempt, offer.sdp.clone(), tx_id))
        })?;
        if marked {
            Ok(())
        } else {
            Err(self.lost_slot(attempt)?)
        }
    }

    /// Apply `Answering(false) -> Answering(true)` to `attempt`; returns whether it applied.
    pub(in crate::swarm) fn mark_reply_sent(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.with_connection_lifecycle(|| Ok(self.peer_lifecycles()?.mark_reply_sent(attempt)))
    }

    /// Reserve an overlay connection generation for `peer` and produce its offer; see
    /// [`Self::prepare_connection_offer_owned_by`].
    pub(in crate::swarm) async fn prepare_connection_offer_with_attempt(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        self.prepare_connection_offer_owned_by(peer, callback, OfferOwner::Overlay)
            .await
    }

    /// Reserve a connection generation for `peer`, driven by `owner`, and produce its offer;
    /// the attempt names the generation so the caller can accept or cancel exactly what it
    /// reserved. The attempt stays `Creating` until its owner marks the offer sent.
    ///
    /// A crossing offer that arrives while this one is created takes the slot (there is no tie
    /// yet); the failure is then `ConnectionAttemptSuperseded`, not the transport error the
    /// interrupted ICE gathering surfaced as.
    pub(in crate::swarm) async fn prepare_connection_offer_owned_by(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        owner: OfferOwner,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        let attempt = self
            .reserve_pending_connection_for(peer, Handshake::offering(owner))
            .await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        let pending_connection = self.new_pending_connection(attempt, callback).await?;
        let attempt = pending_connection.attempt();
        let conn = pending_connection.connection();

        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.webrtc_connection_state(),
            "connection offer create start"
        );
        let offer = match conn.connection.webrtc_create_offer().await {
            Ok(offer) => offer,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection offer create failed"
                );
                return Err(self
                    .classify_step_failure(
                        attempt,
                        Error::Transport(error),
                        "creating connection offer",
                    )
                    .await?);
            }
        };
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = offer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection offer create complete"
        );
        let sdp = match serde_json::to_string(&offer) {
            Ok(sdp) => sdp,
            Err(_) => {
                return Err(self
                    .classify_step_failure(
                        attempt,
                        Error::SerializeToString,
                        "serializing connection offer",
                    )
                    .await?);
            }
        };
        Ok((attempt, ConnectNodeSend {
            sdp,
            dht_protocol_mode: self.dht_protocol_mode(),
        }))
    }

    /// Decide what an offer from `peer` meets here: `None` when the offer may take the slot
    /// (to be answered), or `Some` of this node's own prevailing offer to send back; any record
    /// the offer must not displace refuses it as `AlreadyConnected`. A pending attempt is
    /// decided by `Law (Crossing offer)`.
    async fn reconcile_incoming_offer_peer(&self, peer: Did) -> Result<Option<CounterOffer>> {
        self.expire_pending_connections().await?;
        match self.incoming_offer_admitted_peer(peer)? {
            IncomingOfferAdmittedPeer::Vacant => {}
            IncomingOfferAdmittedPeer::Routable => return Err(Error::AlreadyConnected),
            IncomingOfferAdmittedPeer::Unroutable(attempt) => {
                if self.disconnect_unavailable(attempt).await?.is_none()
                    && self.has_active_connection(peer)
                {
                    return Err(Error::AlreadyConnected);
                }
            }
        }

        match self.unadmitted_record(peer)? {
            None => {}
            Some((_, None)) => return Err(Error::AlreadyConnected),
            Some((attempt, Some(handshake))) => {
                match crossing_offer_verdict(&handshake, self.dht.did, peer) {
                    CrossingOfferVerdict::Refuse => return Err(Error::AlreadyConnected),
                    CrossingOfferVerdict::CounterOffer { sdp, tx_id } => {
                        return Ok(Some(CounterOffer {
                            offer: ConnectNodeSend {
                                sdp,
                                dht_protocol_mode: self.dht_protocol_mode(),
                            },
                            tx_id,
                        }));
                    }
                    CrossingOfferVerdict::Yield => {
                        if !self.cancel_unadmitted_connection(attempt).await? {
                            return Err(Error::AlreadyConnected);
                        }
                    }
                }
            }
        }

        // A raw connection without a lifecycle owner is stale physical state and is removed
        // only by exact identity.
        if let Some(swarm_conn) = self.get_raw_connection(peer) {
            match self.raw_connection_owner(peer)? {
                RawConnectionOwner::Orphan => {
                    if !self
                        .transport
                        .close_connection_if_current(&swarm_conn.connection)
                        .await
                        .map_err(Error::Transport)?
                    {
                        return Err(Error::AlreadyConnected);
                    }
                }
                RawConnectionOwner::Pending(_) | RawConnectionOwner::Owned => {
                    return Err(Error::AlreadyConnected);
                }
            }
        }
        Ok(None)
    }

    /// `peer`'s unadmitted generation, if any, with its signaling state when it is pending
    /// (`None` when it is admitting), read under the lifecycle boundary.
    fn unadmitted_record(
        &self,
        peer: Did,
    ) -> Result<Option<(PendingConnectionAttempt, Option<Handshake>)>> {
        self.with_connection_lifecycle(|| {
            let lifecycles = self.peer_lifecycles()?;
            Ok(lifecycles
                .unadmitted_attempt(peer)
                .map(|attempt| (attempt, lifecycles.handshake(attempt).cloned())))
        })
    }

    async fn create_connection_answer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer: String,
    ) -> Result<CreatedAnswer> {
        let attempt = self
            .reserve_pending_connection_for(peer, Handshake::answering())
            .await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        let pending_connection = self.new_pending_connection(attempt, callback).await?;
        let attempt = pending_connection.attempt();
        let conn = pending_connection.connection();

        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            offer_sdp_bytes = offer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer create start"
        );
        let answer = match conn.connection.webrtc_answer_offer(offer).await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection answer create failed"
                );
                return Err(self
                    .classify_step_failure(
                        attempt,
                        Error::Transport(error),
                        "creating connection answer",
                    )
                    .await?);
            }
        };
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer create complete"
        );
        let sdp = match serde_json::to_string(&answer) {
            Ok(sdp) => sdp,
            Err(_) => {
                return Err(self
                    .classify_step_failure(
                        attempt,
                        Error::SerializeToString,
                        "serializing connection answer",
                    )
                    .await?);
            }
        };
        Ok(CreatedAnswer {
            attempt,
            report: ConnectNodeReport {
                sdp,
                dht_protocol_mode: self.dht_protocol_mode(),
            },
        })
    }

    /// Reply to the offer of remote connection: answer it, or send back this node's own
    /// prevailing offer (`Law (Tie-break)`). The caller marks a delivered answer sent
    /// ([`Self::mark_reply_sent`]) or abandons its generation.
    pub(crate) async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<OfferOutcome> {
        if !self.accepts_connection_offer(offer_msg) {
            return Err(Error::InvalidMessage(
                "connection offer DHT protocol mismatch".to_string(),
            ));
        }
        let offer: String = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;
        match self.reconcile_incoming_offer_peer(peer).await? {
            None => self
                .create_connection_answer(peer, callback, offer)
                .await
                .map(OfferReply::Answer),
            Some(counter_offer) => {
                tracing::debug!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    tx_id = %counter_offer.tx_id,
                    "crossing connection offer refused on the tie-break; counter-offer"
                );
                Ok(OfferReply::CounterOffer(counter_offer))
            }
        }
    }

    /// Accept the answer of remote connection.
    ///
    /// With `expected`, the answer is applied only to that generation: a slot owned by another
    /// generation in any phase (the peer's own offer superseded ours, pending, admitting or
    /// already admitted) is refused as `ConnectionAttemptSuperseded` before the transport is
    /// touched; otherwise, when no pending record with a transport object exists (the
    /// generation was cancelled or expired, or is past pending), `SwarmMissTransport`. The
    /// slot is read once, so one history classifies one way.
    ///
    /// The generation moves `Sent -> Answered` before the answer touches the transport, so a
    /// crossing offer arriving meanwhile is refused rather than countered. With `tx_id` the
    /// answer must echo the transaction the offer left under (an answer over the DHT); without
    /// it (an answer carried out of band) an offer still `Creating` is accepted as well.
    pub(crate) async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
        expected: Option<PendingConnectionAttempt>,
        tx_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        if !self.accepts_connection_answer(answer_msg) {
            return Err(Error::InvalidMessage(
                "connection answer DHT protocol mismatch".to_string(),
            ));
        }

        let answer: String = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        let (attempt, conn) = match (expected, self.answer_slot(peer)?) {
            (Some(expected), AnswerSlot::Pending(owner, _) | AnswerSlot::Owned(owner))
                if owner != expected =>
            {
                return Err(Error::ConnectionAttemptSuperseded {
                    peer,
                    generation: expected.generation,
                });
            }
            (_, AnswerSlot::Pending(attempt, conn)) => (attempt, conn),
            (_, AnswerSlot::Vacant | AnswerSlot::Owned(_)) => {
                return Err(Error::SwarmMissTransport(peer));
            }
        };
        let answered = self.with_connection_lifecycle(|| {
            Ok(self.peer_lifecycles()?.mark_offer_answered(attempt, tx_id))
        })?;
        if !answered {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: attempt.generation,
            });
        }
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer accept start"
        );
        if let Err(error) = conn.connection.webrtc_accept_answer(answer).await {
            self.abandon_pending_connection(attempt, "accepting connection answer")
                .await;
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %peer,
                error = ?error,
                "connection answer accept failed"
            );
            return Err(Error::Transport(error));
        }
        if !self.is_current_connection_attempt(attempt)? {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: attempt.generation,
            });
        }
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.webrtc_connection_state(),
            "connection answer accept complete"
        );

        Ok(())
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod test_handshake;
