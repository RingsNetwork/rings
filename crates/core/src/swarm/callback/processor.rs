use std::sync::Arc;
use std::sync::Mutex;

use futures::lock::Mutex as FuturesMutex;

use super::inbound;
use super::inbound::ReassemblyClock;
use super::pre_admission::PreAdmissionHold;
use super::HeldInboundFrame;
use super::InboundFrameLease;
use super::InboundGate;
use super::InboundLane;
use super::InboundProcessor;
use super::LogicalInbound;
use super::PreparedInboundFrame;
use super::SharedSwarmCallback;
use crate::chunk::MessageReassembler;
use crate::dht::Did;
use crate::measure::Authentication;
use crate::message::Message;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::swarm::session_link::ReferencedSessions;
use crate::swarm::session_link::Swept;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::SESSION_HOLD_TIMEOUT;

fn log_inbound_verification_failure(
    peer: Option<Did>,
    payload: &MessagePayload,
    wire_bytes: usize,
) {
    let message_kind = MessageKind::from_wire(&payload.transaction.data)
        .ok()
        .map(MessageKind::as_str);
    tracing::error!(
        target: "rings_core::swarm::callback",
        peer = ?peer,
        tx_id = %payload.transaction.tx_id,
        destination = %payload.transaction.destination,
        message_kind,
        data_bytes = payload.transaction.data.len(),
        wire_bytes,
        "inbound message verification failed or expired"
    );
}

/// Why the link stage drops a held frame and charges the peer for it: the peer did not back a
/// session it referenced. See the charging law of [`link_stage`](super::link_stage).
#[derive(Clone, Copy, Debug)]
pub(super) enum HeldFrameDrop {
    /// The peer disclaimed the session.
    Disclaimed,
    /// The peer announced a delegation that does not verify.
    AnnouncementRefused,
    /// The peer was asked and did not answer within the hold timeout.
    HoldTimeout,
}

impl std::fmt::Display for HeldFrameDrop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Disclaimed => "the peer disclaimed the session it referenced",
            Self::AnnouncementRefused => "the peer announced a delegation that does not verify",
            Self::HoldTimeout => "the peer did not supply the session within the hold timeout",
        })
    }
}

impl InboundProcessor {
    pub(super) fn new(
        transport: Arc<SwarmTransport>,
        callback: SharedSwarmCallback,
        reassembly_clock: ReassemblyClock,
    ) -> Self {
        let reassembler = MessageReassembler::with_limits_and_budget(
            transport.reassembly_limits(),
            transport.reassembly_budget(),
        );
        Self {
            logical: LogicalInbound::new(transport, callback),
            reassembler: Arc::new(FuturesMutex::new(reassembler)),
            reassembly_clock,
            pending_attempt: Arc::new(Mutex::new(None)),
            pre_admission: Arc::new(Mutex::new(PreAdmissionHold::new(inbound::peer_capacity()))),
            session_link: Arc::new(Mutex::new(ReferencedSessions::new(
                super::SESSION_HOLD_CAPACITY,
                SESSION_HOLD_TIMEOUT.as_millis(),
            ))),
        }
    }

    /// The instant every link step is judged at: the inbound clock, so a test can drive the
    /// hold timeout deterministically.
    pub(super) fn now_ms(&self) -> u128 {
        self.reassembly_clock.now_ms()
    }

    /// The authentication of `peer` as of now: authenticated iff it is the peer of the
    /// handshake this callback is bound to and that handshake's generation is still active.
    /// An unparsable peer is unauthenticated.
    pub(super) fn authentication_of(&self, peer: Option<Did>) -> Authentication {
        let (Some(peer), Some(attempt)) = (peer, self.pending_attempt()) else {
            return Authentication::Unauthenticated;
        };
        if attempt.is_with(peer) && self.logical.transport.is_active_connection_attempt(attempt) {
            Authentication::Authenticated
        } else {
            Authentication::Unauthenticated
        }
    }

    /// Charge `peer` one receive failure under its authentication as of now.
    pub(super) async fn record_receive_failure_now(&self, peer: Option<Did>) {
        let authentication = self.authentication_of(peer);
        self.record_receive_failure(peer, authentication).await;
    }

    /// Charge `peer` one receive failure per frame in `dropped`, all under its authentication
    /// as of now, each for `reason`: the charging law of [`link_stage`](super::link_stage) in
    /// one place. Dropping the leases is the other effect.
    pub(super) async fn charge_dropped_frames(
        &self,
        peer: Option<Did>,
        dropped: Vec<InboundFrameLease>,
        reason: HeldFrameDrop,
    ) {
        let authentication = self.authentication_of(peer);
        let count = dropped.len();
        drop(dropped);
        for _ in 0..count {
            tracing::debug!(peer = ?peer, "dropping message: {reason}");
            self.record_receive_failure(peer, authentication).await;
        }
    }

    /// Drop every frame held past the session-hold timeout, or whose proof lapsed, as of
    /// `now_ms`, charging each to the link's peer: it referenced a session it did not back in
    /// time. The link's peer is the bound handshake's, the one identity every held frame was
    /// admitted to the hold under (see the link law of
    /// [`link_stage`](super::link_stage)).
    pub(super) async fn sweep_session_hold_at(&self, now_ms: u128) {
        let Swept {
            unanswered,
            unasked,
        } = self.session_link().sweep(now_ms);
        let peer = self.pending_attempt().map(PendingConnectionAttempt::peer);
        if !unasked.is_empty() {
            tracing::debug!(
                peer = ?peer,
                dropped = unasked.len(),
                "dropping messages held for a session this end never managed to ask about"
            );
            drop(unasked);
        }
        if !unanswered.is_empty() {
            self.charge_dropped_frames(peer, unanswered, HeldFrameDrop::HoldTimeout)
                .await;
        }
    }

    /// The receiving end of this connection's session references.
    ///
    /// Lock law: held for one pure step and never across a suspension point; a poisoned lock
    /// still guards a well-formed state, since no step panics between two writes.
    pub(super) fn session_link(
        &self,
    ) -> std::sync::MutexGuard<'_, ReferencedSessions<InboundFrameLease>> {
        self.session_link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn pre_admission(
        &self,
    ) -> std::sync::MutexGuard<'_, PreAdmissionHold<HeldInboundFrame>> {
        self.pre_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forget every frame held for a handshake that will never be admitted.
    pub(super) fn discard_pre_admission_hold(&self) {
        let discarded = self.pre_admission().discard();
        if discarded > 0 {
            tracing::debug!("discarded {discarded} frames held for a cancelled pending connection");
        }
    }

    pub(super) fn pending_attempt(&self) -> Option<PendingConnectionAttempt> {
        *self
            .pending_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn set_pending_attempt(&self, attempt: PendingConnectionAttempt) {
        *self
            .pending_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attempt);
    }

    pub(super) async fn record_receive_failure(
        &self,
        peer: Option<Did>,
        authentication: Authentication,
    ) {
        if let Some(peer) = peer {
            self.logical
                .transport
                .record_peer_message_receive_failed(peer, authentication)
                .await;
        }
    }
}

impl InboundProcessor {
    pub(super) async fn pending_connection_gate(
        &self,
        peer: Option<Did>,
    ) -> crate::error::Result<InboundGate> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(InboundGate::Admitted);
        };
        let Some(peer) = peer else {
            tracing::warn!(
                "ignoring message from unparsable peer; pending attempt belongs to {}",
                attempt.peer()
            );
            return Ok(InboundGate::Refused);
        };
        if !attempt.is_with(peer) {
            tracing::warn!(
                "ignoring message from {peer}; pending attempt belongs to {}",
                attempt.peer()
            );
            self.logical
                .transport
                .cancel_unadmitted_connection(attempt)
                .await?;
            self.discard_pre_admission_hold();
            return Ok(InboundGate::Refused);
        }
        if self.logical.transport.is_active_connection_attempt(attempt) {
            Ok(InboundGate::Admitted)
        } else if self
            .logical
            .transport
            .is_current_connection_attempt(attempt)?
        {
            Ok(InboundGate::Unadmitted)
        } else {
            Ok(InboundGate::Superseded)
        }
    }

    /// Whether a frame from `peer` may be dispatched now: the gate judged as of this instant,
    /// with an unadmitted or refused frame both answering no.
    pub(super) async fn pending_connection_admits(
        &self,
        peer: Option<Did>,
    ) -> crate::error::Result<bool> {
        Ok(self.pending_connection_gate(peer).await? == InboundGate::Admitted)
    }

    /// Whether the handshake bound to this callback, if any, has been admitted by now.
    pub(super) fn pending_attempt_admitted(&self) -> bool {
        self.pending_attempt()
            .is_none_or(|attempt| self.logical.transport.is_active_connection_attempt(attempt))
    }

    pub(super) async fn decode_verified_payload(
        &self,
        peer: Option<Did>,
        authentication: Authentication,
        msg: &[u8],
    ) -> crate::error::Result<MessagePayload> {
        let payload = match MessagePayload::from_wire(msg) {
            Ok(payload) => payload,
            Err(e) => {
                self.record_receive_failure(peer, authentication).await;
                return Err(e);
            }
        };
        let network_id = self.logical.transport.network_id;
        if !payload.verify_transaction_and_payload(network_id) {
            log_inbound_verification_failure(peer, &payload, msg.len());
            self.record_receive_failure(peer, authentication).await;
            return Err(crate::error::Error::InvalidMessage(
                "message verification failed or message expired".to_string(),
            ));
        }
        Ok(payload)
    }

    pub(super) async fn validate_preverified_payload(
        &self,
        peer: Option<Did>,
        authentication: Authentication,
        payload: &MessagePayload,
    ) -> crate::error::Result<()> {
        if payload.is_expired() || payload.transaction.is_expired() {
            self.record_receive_failure(peer, authentication).await;
            return Err(crate::error::Error::InvalidMessage(
                "message expired after transport admission".to_string(),
            ));
        }
        Ok(())
    }

    pub(super) async fn accept_verified_logical_message(
        &self,
        peer: Option<Did>,
        authentication: Authentication,
        payload: MessagePayload,
    ) -> crate::error::Result<MessagePayload> {
        self.validate_preverified_payload(peer, authentication, &payload)
            .await?;
        let useful_bytes = u64::try_from(payload.transaction.data.len())
            .map_err(|_| crate::error::Error::MessageSizeOverflow)?;
        if let (Some(peer), Some(attempt)) = (peer, self.pending_attempt()) {
            if attempt.is_with(peer) {
                self.logical
                    .transport
                    .record_peer_message_received(attempt, authentication, useful_bytes)
                    .await;
            }
        }
        Ok(payload)
    }
}

/// Verify one resolved frame of `wire_bytes` bytes and decode the message it carries.
///
/// The payload is self-contained by now: whether a session slot travelled inline or by
/// reference, this is the verification it always was.
pub(super) fn prepare_resolved_frame(
    network_id: u32,
    peer: Option<Did>,
    payload: MessagePayload,
    wire_bytes: usize,
) -> crate::error::Result<PreparedInboundFrame> {
    if !payload.verify_transaction_and_payload(network_id) {
        log_inbound_verification_failure(peer, &payload, wire_bytes);
        return Err(crate::error::Error::InvalidMessage(
            "message verification failed or message expired".to_string(),
        ));
    }
    let message = payload.transaction.data::<Message>()?;
    let kind = MessageKind::from_message(&message);
    let lane = InboundLane::from_kind(kind);
    Ok(PreparedInboundFrame {
        payload,
        message,
        kind,
        lane,
    })
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn prepare_transport_frame_lane_for_test(
    network_id: u32,
    bytes: &[u8],
) -> crate::error::Result<InboundLane> {
    let payload = MessagePayload::from_wire(bytes)?;
    prepare_resolved_frame(network_id, None, payload, bytes.len()).map(|prepared| prepared.lane)
}
