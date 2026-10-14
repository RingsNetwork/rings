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

use std::future::Future;

use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;

use super::pending::crossing_offer_verdict;
use super::pending::AnswerSlot;
use super::pending::CrossingOfferVerdict;
use super::pending::Handshake;
use super::pending::OfferOwner;
use super::pending::PendingConnectionAttempt;
use super::pending::PendingTransportConnection;
use super::pending::RawConnectionOwner;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::swarm::callback::SwarmTransportCallback;

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

    /// Reserve an overlay connection generation for `peer` and produce its offer without
    /// sending it, so a test can deliver it by hand; see
    /// [`Self::prepare_connection_offer_owned_by`].
    #[cfg(test)]
    pub(in crate::swarm) async fn prepare_connection_offer_with_attempt(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        self.prepare_connection_offer_owned_by(peer, callback, OfferOwner::Overlay)
            .await
    }

    /// Reserve a connection generation for `peer`, driven by `owner`, and produce its offer;
    /// the attempt names the generation so the caller can accept or cancel exactly what it
    /// reserved. The attempt stays `Creating` until its owner marks the offer sent.
    pub(in crate::swarm) async fn prepare_connection_offer_owned_by(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        owner: OfferOwner,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        let pending = self
            .open_pending_connection(peer, callback, Handshake::offering(owner))
            .await?;
        let offer = self.create_offer(&pending).await?;
        Ok((pending.attempt(), offer))
    }

    /// Reserve a generation for `peer` whose signaling starts in `handshake`, and create its
    /// transport connection: local work only.
    pub(super) async fn open_pending_connection(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        handshake: Handshake,
    ) -> Result<PendingTransportConnection> {
        let attempt = self.reserve_pending_connection_for(peer, handshake).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        self.new_pending_connection(attempt, callback).await
    }

    /// Create the offer of `pending`, which waits on ICE gathering.
    ///
    /// A crossing offer that arrives while this one is created takes the slot (there is no tie
    /// yet); the failure is then `ConnectionAttemptSuperseded`, not the transport error the
    /// interrupted ICE gathering surfaced as.
    pub(super) async fn create_offer(
        &self,
        pending: &PendingTransportConnection,
    ) -> Result<ConnectNodeSend> {
        let offer = pending.connection().connection.webrtc_create_offer();
        Ok(ConnectNodeSend {
            sdp: self.negotiate_sdp(pending, "offer", offer).await?,
            dht_protocol_mode: self.dht_protocol_mode(),
        })
    }

    /// Create the answer of `pending` to `offer`, which waits on ICE gathering.
    pub(super) async fn create_answer(
        &self,
        pending: &PendingTransportConnection,
        offer: String,
    ) -> Result<ConnectNodeReport> {
        let answer = pending.connection().connection.webrtc_answer_offer(offer);
        Ok(ConnectNodeReport {
            sdp: self.negotiate_sdp(pending, "answer", answer).await?,
            dht_protocol_mode: self.dht_protocol_mode(),
        })
    }

    /// Run `negotiate`, which produces the `side` ("offer" or "answer") of `pending` and waits on
    /// ICE gathering, and serialize its description. A failure reports why
    /// ([`Self::classify_step_failure`]): a generation that lost its slot meanwhile says how,
    /// and a current one is abandoned.
    async fn negotiate_sdp<T, E>(
        &self,
        pending: &PendingTransportConnection,
        side: &'static str,
        negotiate: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<String>
    where
        T: serde::Serialize,
        E: std::fmt::Debug + Into<Error>,
    {
        let attempt = pending.attempt();
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            state = ?pending.connection().webrtc_connection_state(),
            side,
            "connection description create start"
        );
        let description = match negotiate.await {
            Ok(description) => description,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %attempt.peer,
                    generation = attempt.generation,
                    error = ?error,
                    side,
                    "connection description create failed"
                );
                let operation = format!("creating connection {side}");
                return Err(self
                    .classify_step_failure(attempt, error.into(), &operation)
                    .await?);
            }
        };
        match serde_json::to_string(&description) {
            Ok(serialized) => Ok(serialized),
            Err(_) => {
                let operation = format!("serializing connection {side}");
                Err(self
                    .classify_step_failure(attempt, Error::SerializeToString, &operation)
                    .await?)
            }
        }
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
                    && self.active_attempt(peer)?.is_some()
                {
                    return Err(Error::AlreadyConnected);
                }
            }
        }

        // Law (Decided state): a yield releases the attempt only in the signaling state its
        // verdict read. Between the read and the release the attempt may move on (an offer still
        // `Creating` may leave as `Sent`), and a yield decided on the earlier state would then
        // cancel an offer already on its way, while the peer, meeting that offer, yields too:
        // both ends would release the edge. A release that finds the state moved on decides
        // again on the new state. Each retry follows a transition of the attempt, whose phases
        // only advance (`Creating ≺ Sent ≺ Answered`) until it is released, so the loop ends.
        loop {
            let Some((attempt, handshake)) = self.unadmitted_record(peer)? else {
                break;
            };
            let Some(handshake) = handshake else {
                return Err(Error::AlreadyConnected);
            };
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
                    if self.withdraw_pending_in(attempt, handshake).await? {
                        break;
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

    /// Validate `offer_msg` from `peer` and decide it against the peer's slot: local work only.
    /// Either the answering generation is opened, returned with the decoded offer, or this
    /// node's own prevailing offer is returned to send back (`Law (Tie-break)`).
    pub(super) async fn open_answer(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<OfferReply<(PendingTransportConnection, String), CounterOffer>> {
        if !self.accepts_connection_offer(offer_msg) {
            return Err(Error::InvalidMessage(
                "connection offer DHT protocol mismatch".to_string(),
            ));
        }
        let offer: String = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;
        if let Some(counter_offer) = self.reconcile_incoming_offer_peer(peer).await? {
            tracing::debug!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %peer,
                tx_id = %counter_offer.tx_id,
                "crossing connection offer refused on the tie-break; counter-offer"
            );
            return Ok(OfferReply::CounterOffer(counter_offer));
        }
        let pending = self
            .open_pending_connection(peer, callback, Handshake::answering())
            .await?;
        Ok(OfferReply::Answer((pending, offer)))
    }

    /// Reply to the offer of remote connection, waiting on ICE gathering (the application
    /// boundary): answer it, or return this node's own prevailing offer (`Law (Tie-break)`).
    /// The caller marks a delivered answer sent ([`Self::mark_reply_sent`]) or abandons its
    /// generation.
    pub(in crate::swarm) async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<OfferOutcome> {
        match self.open_answer(peer, callback, offer_msg).await? {
            OfferReply::Answer((pending, offer)) => {
                let report = self.create_answer(&pending, offer).await?;
                Ok(OfferReply::Answer(CreatedAnswer {
                    attempt: pending.attempt(),
                    report,
                }))
            }
            OfferReply::CounterOffer(counter_offer) => Ok(OfferReply::CounterOffer(counter_offer)),
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
