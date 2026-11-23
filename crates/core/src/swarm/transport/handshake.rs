//! The connection handshake: multi-step `handshake_step` (`0..MAX_HANDSHAKE_STEPS`), crossing
//! initial offers, and recorded-path signaling (#945, #947, #1291).
//!
//! An offer reserves a pending generation whose signaling state ([`Handshake`]) records who
//! drives it and how far it has got; the state lives and dies with the pending record. Two
//! crossing initial offers (`step_index == 0`) are decided by `Law (Crossing offer)` (see
//! `pending::handshake`): an offer still being created yields, an offer carried out of band
//! prevails over a sent overlay offer, and of two sent overlay offers the smaller DID's prevails
//! and is sent back to the peer as a counter-offer. Over the DHT, offers record their path and
//! every reply or continuation retraces it (#947, see the `signaling` module), so an edge both
//! ends want is established whenever either overlay offer is delivered.
//!
//! A transport step of an attempt that lost its slot reports why ([`classify_step_failure`]):
//! `ConnectionAttemptSuperseded` when the peer's own handshake took the slot, and
//! `ConnectionAttemptCancelled` when anything else released it. A yield replaces the yielding
//! generation with the answering one in one lifecycle transition (`Law (Atomic yield)`), so the
//! slot is never vacant in between and a yielded offer always reports
//! `ConnectionAttemptSuperseded` (#954).
//!
//! [`classify_step_failure`]: SwarmTransport::classify_step_failure

use std::future::Future;

use bytes::Bytes;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::HandshakeStep;
use rings_transport::core::transport::MAX_HANDSHAKE_STEPS;
use rings_transport::core::transport::MIN_MAX_MESSAGE_SIZE;

use super::pending::crossing_offer_verdict;
use super::pending::AnswerSlot;
use super::pending::CrossingOfferVerdict;
use super::pending::Handshake;
use super::pending::OfferOwner;
use super::pending::OfferPhase;
use super::pending::PendingConnectionAttempt;
use super::pending::PendingTransportConnection;
use super::pending::RawConnectionOwner;
use super::SwarmConnection;
use super::SwarmTransport;
use crate::consts::MAX_HANDSHAKE_BYTES;
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

/// Result of driving step `0` (`handshake_step(None)`) on an initiator connection.
#[derive(Clone, Debug)]
pub(crate) enum InitialOfferStep {
    /// 0-step transport (`HandshakeStep::Done`): handshake completed locally with no outbound
    /// signaling message.
    Completed,
    /// 1+-step transport (`HandshakeStep::Send` or `SendFinal`): outbound step `0` to send.
    Send(ConnectNodeSend),
}

/// Result of driving a responder step (`handshake_step(Some(inbound))`).
#[derive(Clone, Debug)]
pub(crate) enum ResponderStepOutcome {
    /// Responder completed (`1`-step or final even continuation step) with no outbound reply.
    Completed,
    /// Responder produced an outbound `ConnectNodeReport` (`Send` or `SendFinal`).
    Reply(CreatedAnswer),
}

/// Result of driving an initiator step `>= 1` (`handshake_step(Some(report))`).
#[derive(Clone, Debug)]
pub(crate) enum InitiatorReplyOutcome {
    /// Initiator completed (`HandshakeStep::Done`).
    Completed,
    /// Initiator produced an even continuation step (`ConnectNodeSend` with `step_index >= 2`)
    /// for the claimed `PendingConnectionAttempt`.
    Continue {
        /// Initiator generation owning this multi-step handshake.
        attempt: PendingConnectionAttempt,
        /// Next even outbound `ConnectNodeSend` step.
        offer: ConnectNodeSend,
    },
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
pub(crate) type OfferOutcome = OfferReply<Option<CreatedAnswer>, CounterOffer>;

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

/// What an incoming offer's reconciliation leaves in the peer's slot.
#[derive(Clone, Debug)]
enum IncomingOfferSlot {
    /// No record: the answering generation is still to be reserved.
    Vacant,
    /// This node's creating or sent offer yielded; the answering generation already owns the
    /// slot (`Law (Atomic yield)`).
    Yielded(PendingConnectionAttempt),
    /// This node's own offer prevails and is sent back (`Law (Tie-break)`).
    CounterOffer(CounterOffer),
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
    pub(in crate::swarm) async fn classify_step_failure(
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

    /// Await `step` of `attempt`'s handshake (wrapping or sending an offer or an answer); a
    /// failure abandons the generation, logged as `operation`, so the peer's next offer is not
    /// refused for the pending timeout, and is returned.
    pub(in crate::swarm) async fn settle_handshake_step<T>(
        &self,
        attempt: PendingConnectionAttempt,
        operation: &str,
        step: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        match step.await {
            Ok(value) => Ok(value),
            Err(error) => Err(self
                .classify_step_failure(attempt, error, operation)
                .await?),
        }
    }

    /// Effective maximum byte length permitted for a single handshake step on this transport.
    pub(super) fn max_handshake_step_bytes(&self) -> usize {
        self.transport
            .max_handshake_step_bytes()
            .min(MAX_HANDSHAKE_BYTES)
    }

    /// Symmetrically validate an inbound handshake step's index, byte bound, and backend
    /// pre-validation hook before mutating or allocating any connection slot.
    pub(super) fn validate_inbound_handshake_step(
        &self,
        step_index: u8,
        payload: &Bytes,
    ) -> Result<()> {
        if step_index >= MAX_HANDSHAKE_STEPS {
            return Err(Error::InvalidMessage(format!(
                "handshake step_index {step_index} exceeds MAX_HANDSHAKE_STEPS ({MAX_HANDSHAKE_STEPS})"
            )));
        }
        if payload.len() > self.max_handshake_step_bytes() {
            return Err(Error::HandshakeAboveMax(payload.len()));
        }
        self.transport
            .validate_inbound_handshake(step_index, payload)
            .map_err(Error::Transport)
    }

    /// Enforce the protocol `MIN_MAX_MESSAGE_SIZE` floor on a connection that has finished its
    /// handshake steps, abandoning `attempt` if the backend negotiated a smaller limit.
    pub(super) async fn ensure_negotiated_max_message_size_or_abandon(
        &self,
        attempt: PendingConnectionAttempt,
        conn: &SwarmConnection,
        operation: &str,
    ) -> Result<()> {
        let advertised = conn.max_message_size();
        if advertised < MIN_MAX_MESSAGE_SIZE {
            let err = Error::Transport(
                rings_transport::error::Error::RemoteMaxMessageSizeTooSmall {
                    advertised,
                    minimum: MIN_MAX_MESSAGE_SIZE,
                },
            );
            if self.is_current_connection_attempt(attempt)? {
                self.abandon_pending_connection(attempt, operation).await;
            } else if self.with_connection_lifecycle(|| {
                Ok(self.peer_lifecycles()?.superseded_by_peer(attempt))
            })? {
                return Err(Error::ConnectionAttemptSuperseded {
                    peer: attempt.peer,
                    generation: attempt.generation,
                });
            }
            return Err(err);
        }
        Ok(())
    }

    /// Apply `Creating -> Sent(0, final_step, ..)` or `ClaimedReply(2k) -> Sent(2k) / Answered` to
    /// `attempt`; when it no longer owns its slot, the error says why ([`Self::lost_slot`]).
    pub(in crate::swarm) fn mark_offer_sent(
        &self,
        attempt: PendingConnectionAttempt,
        offer: &ConnectNodeSend,
        tx_id: uuid::Uuid,
    ) -> Result<()> {
        let marked = self.with_connection_lifecycle(|| {
            if self.peer_lifecycles()?.pending_attempt(attempt.peer) == Some(attempt) {
                Ok(self.peer_lifecycles()?.mark_offer_sent(
                    attempt,
                    offer.step_index,
                    offer.final_step,
                    offer.handshake.clone(),
                    tx_id,
                    offer.session_id,
                ))
            } else if offer.final_step {
                self.is_current_connection_attempt(attempt)
            } else {
                Ok(false)
            }
        })?;
        if marked {
            Ok(())
        } else {
            Err(self.lost_slot(attempt)?)
        }
    }

    /// Reserve an overlay connection generation for `peer` and produce its initial offer without
    /// sending it, so a test can deliver it by hand; see
    /// [`Self::prepare_connection_offer_owned_by`].
    #[cfg(all(
        test,
        any(feature = "dummy", not(all(feature = "wasm", target_family = "wasm")))
    ))]
    pub(in crate::swarm) async fn prepare_connection_offer_with_attempt(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        let (attempt, step) = self
            .prepare_connection_offer_owned_by(peer, callback, OfferOwner::Overlay)
            .await?;
        match step {
            InitialOfferStep::Send(offer) => Ok((attempt, offer)),
            InitialOfferStep::Completed => {
                let err = Error::Transport(rings_transport::error::Error::UnsupportedOperation(
                    "0-step transport produces no initial ConnectNodeSend".to_string(),
                ));
                Err(self
                    .classify_step_failure(attempt, err, "preparing initial connection offer")
                    .await?)
            }
        }
    }

    /// Reserve a connection generation for `peer`, driven by `owner`, and produce its initial
    /// step (`Completed` for a 0-step transport, or `Send(ConnectNodeSend)` for 1+-step
    /// transports). For `Send`, the attempt stays `Creating` until its owner marks the offer sent.
    pub(in crate::swarm) async fn prepare_connection_offer_owned_by(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        owner: OfferOwner,
    ) -> Result<(PendingConnectionAttempt, InitialOfferStep)> {
        let pending = self
            .open_pending_connection(peer, callback, Handshake::offering(owner))
            .await?;
        let attempt = pending.attempt();
        let step = self.create_initial_step(&pending).await?;
        Ok((attempt, step))
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
        self.open_reserved_connection(attempt, callback).await
    }

    /// Create the transport connection of the already reserved generation `attempt`: local
    /// work only.
    pub(super) async fn open_reserved_connection(
        &self,
        attempt: PendingConnectionAttempt,
        callback: SwarmTransportCallback,
    ) -> Result<PendingTransportConnection> {
        let callback = callback.with_pending_connection_attempt(attempt);
        self.new_pending_connection(attempt, callback).await
    }

    /// Drive step `0` (`handshake_step(None)`) on an initiator `pending` connection.
    pub(super) async fn create_initial_step(
        &self,
        pending: &PendingTransportConnection,
    ) -> Result<InitialOfferStep> {
        let attempt = pending.attempt();
        let step = self
            .run_handshake_step(attempt, pending.connection(), None, "offer")
            .await?;
        match step {
            HandshakeStep::Done => {
                self.ensure_negotiated_max_message_size_or_abandon(
                    attempt,
                    pending.connection(),
                    "completing 0-step handshake",
                )
                .await?;
                let still_valid = self.with_connection_lifecycle(|| {
                    if self.peer_lifecycles()?.pending_attempt(attempt.peer) == Some(attempt) {
                        Ok(self.peer_lifecycles()?.mark_zero_step_completed(attempt))
                    } else {
                        self.is_current_connection_attempt(attempt)
                    }
                })?;
                if !still_valid {
                    return Err(self.lost_slot(attempt)?);
                }
                Ok(InitialOfferStep::Completed)
            }
            HandshakeStep::SendFinal(bytes) => {
                if bytes.len() > self.max_handshake_step_bytes() {
                    return Err(self
                        .classify_step_failure(
                            attempt,
                            Error::HandshakeAboveMax(bytes.len()),
                            "checking initial final handshake size",
                        )
                        .await?);
                }
                self.ensure_negotiated_max_message_size_or_abandon(
                    attempt,
                    pending.connection(),
                    "completing 1-step handshake",
                )
                .await?;
                Ok(InitialOfferStep::Send(ConnectNodeSend {
                    step_index: 0,
                    final_step: true,
                    session_id: None,
                    handshake: bytes,
                    dht_protocol_mode: self.dht_protocol_mode(),
                    relay_custody: self.dht.relay_custody(),
                }))
            }
            HandshakeStep::Send(bytes) => {
                if bytes.len() > self.max_handshake_step_bytes() {
                    return Err(self
                        .classify_step_failure(
                            attempt,
                            Error::HandshakeAboveMax(bytes.len()),
                            "checking initial handshake size",
                        )
                        .await?);
                }
                Ok(InitialOfferStep::Send(ConnectNodeSend {
                    step_index: 0,
                    final_step: false,
                    session_id: None,
                    handshake: bytes,
                    dht_protocol_mode: self.dht_protocol_mode(),
                    relay_custody: self.dht.relay_custody(),
                }))
            }
        }
    }

    /// Drive a responder step (`handshake_step(Some(inbound))`) for `attempt` on `conn`.
    pub(super) async fn advance_responder_step(
        &self,
        attempt: PendingConnectionAttempt,
        conn: &SwarmConnection,
        inbound_step: u8,
        inbound_final: bool,
        inbound_session_id: Option<[u8; 16]>,
        inbound: Bytes,
    ) -> Result<ResponderStepOutcome> {
        let step = self
            .run_handshake_step(attempt, conn, Some(inbound), "answer")
            .await?;
        if inbound_final {
            match step {
                HandshakeStep::Done => {
                    self.ensure_negotiated_max_message_size_or_abandon(
                        attempt,
                        conn,
                        "completing responder final step",
                    )
                    .await?;
                    return Ok(ResponderStepOutcome::Completed);
                }
                other => {
                    let err = Error::Transport(
                        rings_transport::error::Error::RemoteHandshakeApplyError(format!(
                            "expected HandshakeStep::Done on final inbound step, got {other:?}"
                        )),
                    );
                    return Err(self
                        .classify_step_failure(attempt, err, "completing responder final step")
                        .await?);
                }
            }
        }

        let outbound_step = inbound_step.saturating_add(1);
        match step {
            HandshakeStep::SendFinal(bytes) => {
                if bytes.len() > self.max_handshake_step_bytes() {
                    return Err(self
                        .classify_step_failure(
                            attempt,
                            Error::HandshakeAboveMax(bytes.len()),
                            "checking responder final handshake size",
                        )
                        .await?);
                }
                self.ensure_negotiated_max_message_size_or_abandon(
                    attempt,
                    conn,
                    "completing responder answer",
                )
                .await?;
                Ok(ResponderStepOutcome::Reply(CreatedAnswer {
                    attempt,
                    report: ConnectNodeReport {
                        step_index: outbound_step,
                        final_step: true,
                        session_id: inbound_session_id,
                        handshake: bytes,
                        dht_protocol_mode: self.dht_protocol_mode(),
                        relay_custody: self.dht.relay_custody(),
                    },
                }))
            }
            HandshakeStep::Send(bytes) => {
                let next_even_step = outbound_step.saturating_add(1);
                if next_even_step >= MAX_HANDSHAKE_STEPS {
                    let err = Error::Transport(
                        rings_transport::error::Error::LocalHandshakeGenerationError(format!(
                            "handshake exceeded MAX_HANDSHAKE_STEPS ({MAX_HANDSHAKE_STEPS})"
                        )),
                    );
                    return Err(self
                        .classify_step_failure(attempt, err, "advancing multi-step responder")
                        .await?);
                }
                if bytes.len() > self.max_handshake_step_bytes() {
                    return Err(self
                        .classify_step_failure(
                            attempt,
                            Error::HandshakeAboveMax(bytes.len()),
                            "checking responder continuation size",
                        )
                        .await?);
                }
                let sid = inbound_session_id.unwrap_or_else(|| *self.rng.next_uuid().as_bytes());
                let recorded = self.with_connection_lifecycle(|| {
                    Ok(self.peer_lifecycles()?.set_answering_awaiting_step(
                        attempt,
                        next_even_step,
                        sid,
                    ))
                })?;
                if !recorded {
                    return Err(self.lost_slot(attempt)?);
                }
                Ok(ResponderStepOutcome::Reply(CreatedAnswer {
                    attempt,
                    report: ConnectNodeReport {
                        step_index: outbound_step,
                        final_step: false,
                        session_id: Some(sid),
                        handshake: bytes,
                        dht_protocol_mode: self.dht_protocol_mode(),
                        relay_custody: self.dht.relay_custody(),
                    },
                }))
            }
            HandshakeStep::Done => {
                let err =
                    Error::Transport(rings_transport::error::Error::RemoteHandshakeApplyError(
                        "unexpected HandshakeStep::Done on non-final inbound step".to_string(),
                    ));
                Err(self
                    .classify_step_failure(
                        attempt,
                        err,
                        "completing responder step on non-final inbound step",
                    )
                    .await?)
            }
        }
    }

    /// Execute one `ConnectionInterface::handshake_step` on `conn` for `attempt`, classifying
    /// supersession vs cancellation if the step fails.
    pub(super) async fn run_handshake_step(
        &self,
        attempt: PendingConnectionAttempt,
        conn: &SwarmConnection,
        inbound: Option<Bytes>,
        side: &'static str,
    ) -> Result<HandshakeStep> {
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            state = ?conn.connection_state(),
            side,
            "connection handshake_step start"
        );
        match conn.connection.handshake_step(inbound).await {
            Ok(step) => Ok(step),
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %attempt.peer,
                    generation = attempt.generation,
                    error = ?error,
                    side,
                    "connection handshake_step failed"
                );
                let operation = format!("executing connection {side}");
                Err(self
                    .classify_step_failure(attempt, Error::Transport(error), &operation)
                    .await?)
            }
        }
    }

    /// Decide what an offer from `peer`, carried by `carrier`, meets here: a vacant slot or a
    /// yielded one whose answering generation is already reserved (to be answered), or this
    /// node's own prevailing offer to send back; any record the offer must not displace refuses
    /// it as `AlreadyConnected`. A pending attempt is decided by `Law (Crossing offer)`.
    async fn reconcile_incoming_offer_peer(
        &self,
        peer: Did,
        carrier: OfferOwner,
    ) -> Result<IncomingOfferSlot> {
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

        // Law (Decided state): a yield replaces the attempt only in the signaling state its
        // verdict read. Between the read and the release the attempt may move on (an offer still
        // `Creating` may leave as `Sent`), and a yield decided on the earlier state would then
        // cancel an offer already on its way, while the peer, meeting that offer, yields too:
        // both ends would release the edge. A release that finds the state moved on decides
        // again on the new state. Each retry follows a transition of the attempt, whose phases
        // only advance (`Creating ≺ Sent ≺ Answered`) until it is released, so the loop ends.
        let mut answering = None;
        loop {
            let Some((attempt, handshake)) = self.unadmitted_record(peer)? else {
                break;
            };
            let Some(handshake) = handshake else {
                return Err(Error::AlreadyConnected);
            };
            match crossing_offer_verdict(&handshake, carrier, self.dht.did, peer) {
                CrossingOfferVerdict::Refuse => return Err(Error::AlreadyConnected),
                CrossingOfferVerdict::CounterOffer {
                    final_step,
                    handshake,
                    session_id,
                    tx_id,
                } => {
                    return Ok(IncomingOfferSlot::CounterOffer(CounterOffer {
                        offer: ConnectNodeSend {
                            step_index: 0,
                            final_step,
                            session_id,
                            handshake,
                            dht_protocol_mode: self.dht_protocol_mode(),
                            relay_custody: self.dht.relay_custody(),
                        },
                        tx_id,
                    }));
                }
                CrossingOfferVerdict::Yield => {
                    answering = self.yield_pending_in(attempt, handshake).await?;
                    if answering.is_some() {
                        break;
                    }
                }
            }
        }

        if let Some(answering) = answering {
            // The yield closed the yielded generation's transport, the only raw connection the
            // slot held, and the answering generation owns the slot.
            return Ok(IncomingOfferSlot::Yielded(answering));
        }

        // A raw connection without a lifecycle owner is stale physical state and is removed
        // only by exact identity.
        if let Some(swarm_conn) = self.get_raw_connection(peer) {
            match self.raw_connection_owner(peer)? {
                RawConnectionOwner::Orphan => {
                    if !self.close_if_current(&swarm_conn).await? {
                        return Err(Error::AlreadyConnected);
                    }
                }
                RawConnectionOwner::Pending(_) | RawConnectionOwner::Owned => {
                    return Err(Error::AlreadyConnected);
                }
            }
        }
        Ok(IncomingOfferSlot::Vacant)
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

    /// Validate an initial (`step_index == 0`) `offer_msg` from `peer`, carried by `carrier`, and
    /// decide it against the peer's slot: local work only. Either the answering generation is
    /// opened, returned with the validated offer payload, or this node's own prevailing offer is
    /// returned to send back (`Law (Tie-break)`).
    pub(super) async fn open_answer(
        &self,
        peer: Did,
        carrier: OfferOwner,
        callback: SwarmTransportCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<OfferReply<(PendingTransportConnection, Bytes), CounterOffer>> {
        if offer_msg.step_index != 0 {
            return Err(Error::InvalidMessage(format!(
                "initial connection offer expected step_index 0, got {}",
                offer_msg.step_index
            )));
        }
        if !self.accepts_connection_offer(offer_msg) {
            return Err(Error::InvalidMessage(
                "connection offer DHT protocol mismatch".to_string(),
            ));
        }
        self.validate_inbound_handshake_step(0, &offer_msg.handshake)?;
        self.dht
            .observe_relay_custody(peer, offer_msg.relay_custody)?;
        let pending = match self.reconcile_incoming_offer_peer(peer, carrier).await? {
            IncomingOfferSlot::CounterOffer(counter_offer) => {
                tracing::debug!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    tx_id = %counter_offer.tx_id,
                    "crossing connection offer refused on the tie-break; counter-offer"
                );
                return Ok(OfferReply::CounterOffer(counter_offer));
            }
            IncomingOfferSlot::Yielded(answering) => {
                self.open_reserved_connection(answering, callback).await?
            }
            IncomingOfferSlot::Vacant => {
                self.open_pending_connection(peer, callback, Handshake::answering())
                    .await?
            }
        };
        Ok(OfferReply::Answer((pending, offer_msg.handshake.clone())))
    }

    /// Reply to an out-of-band connection offer (or even continuation step) from `peer`.
    pub(in crate::swarm) async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: SwarmTransportCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<OfferOutcome> {
        if offer_msg.step_index > 0 {
            if !self.accepts_connection_offer(offer_msg) {
                return Err(Error::InvalidMessage(
                    "connection continuation DHT protocol mismatch".to_string(),
                ));
            }
            if !offer_msg.step_index.is_multiple_of(2) {
                return Err(Error::InvalidMessage(format!(
                    "ConnectNodeSend continuation step_index must be even, got {}",
                    offer_msg.step_index
                )));
            }
            let Some(sid) = offer_msg.session_id else {
                return Err(Error::InvalidMessage(
                    "ConnectNodeSend continuation missing session_id".to_string(),
                ));
            };
            let (attempt, maybe_conn) = self.with_connection_lifecycle(|| {
                let Some(attempt) = self.peer_lifecycles()?.claim_answering_continuation(
                    peer,
                    offer_msg.step_index,
                    sid,
                ) else {
                    return Err(Error::SwarmMissTransport(peer));
                };
                Ok((attempt, self.get_raw_connection(peer)))
            })?;
            let Some(conn) = maybe_conn else {
                self.abandon_pending_connection(attempt, "missing raw connection for continuation")
                    .await;
                return Err(Error::SwarmMissTransport(peer));
            };
            if let Err(err) =
                self.validate_inbound_handshake_step(offer_msg.step_index, &offer_msg.handshake)
            {
                return Err(self
                    .classify_step_failure(attempt, err, "validating continuation offer")
                    .await?);
            }
            return match self
                .advance_responder_step(
                    attempt,
                    &conn,
                    offer_msg.step_index,
                    offer_msg.final_step,
                    Some(sid),
                    offer_msg.handshake.clone(),
                )
                .await?
            {
                ResponderStepOutcome::Completed => Ok(OfferReply::Answer(None)),
                ResponderStepOutcome::Reply(created) => Ok(OfferReply::Answer(Some(created))),
            };
        }

        match self
            .open_answer(peer, OfferOwner::External, callback, offer_msg)
            .await?
        {
            OfferReply::Answer((pending, offer_bytes)) => {
                match self
                    .advance_responder_step(
                        pending.attempt(),
                        pending.connection(),
                        0,
                        offer_msg.final_step,
                        offer_msg.session_id,
                        offer_bytes,
                    )
                    .await?
                {
                    ResponderStepOutcome::Completed => Ok(OfferReply::Answer(None)),
                    ResponderStepOutcome::Reply(created) => Ok(OfferReply::Answer(Some(created))),
                }
            }
            OfferReply::CounterOffer(counter_offer) => Ok(OfferReply::CounterOffer(counter_offer)),
        }
    }

    /// Advance an initiator connection on an inbound `ConnectNodeReport` (`step_index == 1` or odd
    /// continuation reply).
    ///
    /// Returns [`InitiatorReplyOutcome::Completed`] when `answer_msg.final_step` is `true`, or
    /// [`InitiatorReplyOutcome::Continue`] with the next even `ConnectNodeSend` when the responder
    /// requested another round (`answer_msg.final_step == false`).
    pub(crate) async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
        expected: Option<PendingConnectionAttempt>,
        tx_id: Option<uuid::Uuid>,
    ) -> Result<InitiatorReplyOutcome> {
        if !self.accepts_connection_answer(answer_msg) {
            return Err(Error::InvalidMessage(
                "connection answer DHT protocol mismatch".to_string(),
            ));
        }
        if answer_msg.step_index % 2 != 1 {
            return Err(Error::InvalidMessage(format!(
                "ConnectNodeReport step_index must be odd, got {}",
                answer_msg.step_index
            )));
        }

        if !answer_msg.final_step && answer_msg.session_id.is_none() {
            return Err(Error::InvalidMessage(
                "non-final ConnectNodeReport missing session_id".to_string(),
            ));
        }
        if !answer_msg.final_step && answer_msg.step_index.saturating_add(1) >= MAX_HANDSHAKE_STEPS
        {
            return Err(Error::InvalidMessage(format!(
                "handshake step {} exceeds MAX_HANDSHAKE_STEPS ({MAX_HANDSHAKE_STEPS})",
                answer_msg.step_index.saturating_add(1)
            )));
        }

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

        // Atomically claim the inbound reply under the lifecycle lock before running any backend
        // step, verifying that `attempt` is an `Offering` generation awaiting `answer_msg.step_index`
        // under matching `tx_id` / `session_id` (rejecting `Answering` slots and duplicate reports).
        self.with_connection_lifecycle(|| {
            let mut lifecycles = self.peer_lifecycles()?;
            if lifecycles.claim_offer_reply(
                attempt,
                tx_id,
                answer_msg.step_index,
                answer_msg.final_step,
                answer_msg.session_id,
            ) {
                return Ok(());
            }
            match lifecycles.handshake(attempt) {
                Some(Handshake::Offering {
                    phase:
                        OfferPhase::Sent {
                            step_index: sent_step,
                            final_step: false,
                            tx_id: sent_tx,
                            session_id: sent_sid,
                            ..
                        },
                    ..
                }) if tx_id.is_none_or(|t| t == *sent_tx) => {
                    Err(Error::InvalidMessage(format!(
                        "ConnectNodeReport (step {}, session {:?}) does not match pending offer (step {sent_step}, session {sent_sid:?})",
                        answer_msg.step_index, answer_msg.session_id
                    )))
                }
                _ => Err(Error::ConnectionAttemptSuperseded {
                    peer,
                    generation: attempt.generation,
                }),
            }
        })?;

        if let Err(err) =
            self.validate_inbound_handshake_step(answer_msg.step_index, &answer_msg.handshake)
        {
            return Err(self
                .classify_step_failure(attempt, err, "validating connection answer")
                .await?);
        }

        if !answer_msg.final_step {
            let sid = answer_msg.session_id.unwrap_or_default();
            return self
                .advance_claimed_initiator_continuation(attempt, &conn, answer_msg, sid)
                .await;
        }

        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_handshake_bytes = answer_msg.handshake.len(),
            state = ?conn.connection_state(),
            "connection answer accept start"
        );
        match self
            .run_handshake_step(
                attempt,
                &conn,
                Some(answer_msg.handshake.clone()),
                "accepting connection answer",
            )
            .await?
        {
            HandshakeStep::Done => {}
            other => {
                let err =
                    Error::Transport(rings_transport::error::Error::RemoteHandshakeApplyError(
                        format!("expected HandshakeStep::Done on final answer, got {other:?}"),
                    ));
                return Err(self
                    .classify_step_failure(attempt, err, "accepting connection answer")
                    .await?);
            }
        }
        self.ensure_negotiated_max_message_size_or_abandon(
            attempt,
            &conn,
            "verifying negotiated max_message_size on answer",
        )
        .await?;
        self.dht
            .observe_relay_custody(peer, answer_msg.relay_custody)?;
        if !self.is_current_connection_attempt(attempt)? {
            return Err(self.lost_slot(attempt)?);
        }
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.connection_state(),
            "connection answer accept complete"
        );

        Ok(InitiatorReplyOutcome::Completed)
    }

    /// Execute the backend step for an initiator generation whose non-final `ConnectNodeReport`
    /// has already been atomically claimed into `OfferPhase::ClaimedReply`.
    pub(super) async fn advance_claimed_initiator_continuation(
        &self,
        attempt: PendingConnectionAttempt,
        conn: &SwarmConnection,
        answer_msg: &ConnectNodeReport,
        sid: [u8; 16],
    ) -> Result<InitiatorReplyOutcome> {
        let peer = attempt.peer;
        let next_even_step = answer_msg.step_index.saturating_add(1);
        let step = self
            .run_handshake_step(
                attempt,
                conn,
                Some(answer_msg.handshake.clone()),
                "continuation",
            )
            .await?;
        match step {
            HandshakeStep::SendFinal(bytes) => {
                if bytes.len() > self.max_handshake_step_bytes() {
                    return Err(self
                        .classify_step_failure(
                            attempt,
                            Error::HandshakeAboveMax(bytes.len()),
                            "checking initiator final continuation size",
                        )
                        .await?);
                }
                self.ensure_negotiated_max_message_size_or_abandon(
                    attempt,
                    conn,
                    "completing initiator final continuation",
                )
                .await?;
                self.dht
                    .observe_relay_custody(peer, answer_msg.relay_custody)?;
                Ok(InitiatorReplyOutcome::Continue {
                    attempt,
                    offer: ConnectNodeSend {
                        step_index: next_even_step,
                        final_step: true,
                        session_id: Some(sid),
                        handshake: bytes,
                        dht_protocol_mode: self.dht_protocol_mode(),
                        relay_custody: self.dht.relay_custody(),
                    },
                })
            }
            HandshakeStep::Send(bytes) => {
                if next_even_step.saturating_add(1) >= MAX_HANDSHAKE_STEPS {
                    let err = Error::Transport(
                        rings_transport::error::Error::LocalHandshakeGenerationError(format!(
                            "handshake exceeded MAX_HANDSHAKE_STEPS ({MAX_HANDSHAKE_STEPS})"
                        )),
                    );
                    return Err(self
                        .classify_step_failure(attempt, err, "checking multi-step initiator bound")
                        .await?);
                }
                if bytes.len() > self.max_handshake_step_bytes() {
                    return Err(self
                        .classify_step_failure(
                            attempt,
                            Error::HandshakeAboveMax(bytes.len()),
                            "checking initiator continuation size",
                        )
                        .await?);
                }
                self.dht
                    .observe_relay_custody(peer, answer_msg.relay_custody)?;
                Ok(InitiatorReplyOutcome::Continue {
                    attempt,
                    offer: ConnectNodeSend {
                        step_index: next_even_step,
                        final_step: false,
                        session_id: Some(sid),
                        handshake: bytes,
                        dht_protocol_mode: self.dht_protocol_mode(),
                        relay_custody: self.dht.relay_custody(),
                    },
                })
            }
            HandshakeStep::Done => {
                let err = Error::Transport(
                    rings_transport::error::Error::LocalHandshakeGenerationError(
                        "expected Send or SendFinal in response to non-final report, got Done"
                            .to_string(),
                    ),
                );
                Err(self
                    .classify_step_failure(attempt, err, "advancing initiator continuation")
                    .await?)
            }
        }
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod test_handshake;
