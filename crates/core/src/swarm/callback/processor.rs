use std::sync::Arc;
use std::sync::Mutex;

use futures::lock::Mutex as FuturesMutex;

use super::inbound;
use super::inbound::ReassemblyClock;
use super::pre_admission::PreAdmissionHold;
use super::HeldInboundFrame;
use super::InboundGate;
use super::InboundLane;
use super::InboundProcessor;
use super::LogicalInbound;
use super::PreparedInboundFrame;
use crate::chunk::MessageReassembler;
use crate::delegation::DelegatorAuthorization;
use crate::dht::Did;
use crate::measure::Authentication;
use crate::message::EdgeRelation;
use crate::message::LinkWindows;
use crate::message::Message;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PerSlot;
use crate::message::ReplayWindow;
use crate::swarm::session_link::DelegationLessor;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

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

impl InboundProcessor {
    /// The connection stage of one connection over `transport`, judging reassembly timeouts by
    /// `reassembly_clock`, with no connection attempt bound yet and empty holds.
    pub(super) fn new(transport: Arc<SwarmTransport>, reassembly_clock: ReassemblyClock) -> Self {
        let reassembler = MessageReassembler::with_limits_and_budget(
            transport.reassembly_limits(),
            transport.reassembly_budget(),
        );
        Self {
            logical: LogicalInbound::new(transport),
            reassembler: Arc::new(FuturesMutex::new(reassembler)),
            reassembly_clock,
            pending_attempt: Arc::new(Mutex::new(None)),
            pre_admission: Arc::new(Mutex::new(PreAdmissionHold::new(inbound::peer_capacity()))),
            session_link: Arc::new(Mutex::new(DelegationLessor::new())),
            link_windows: Arc::new(FuturesMutex::new(LinkWindows::new())),
        }
    }

    /// Commit the replay and origin-quota admission of a transaction addressed to this node in
    /// the window its classification selects (#1081; durable-records, Law Window by
    /// classification): this connection's volatile window for a link-local transaction
    /// (Application traffic of a namespace that delegated its replay, from the authenticated
    /// neighbour that originated it), the durable window for every other transaction.
    pub(super) async fn admit_final_transaction(
        &self,
        payload: &MessagePayload,
        message: &Message,
        lane: InboundLane,
        edge: EdgeRelation,
    ) -> crate::error::Result<()> {
        let class = lane
            .class()
            .ok_or(crate::error::Error::InboundActorInvariantViolation)?;
        match ReplayWindow::select(class, edge, || self.logical.delegates_replay(message)) {
            ReplayWindow::Durable => {
                self.logical
                    .admit_final_transaction(payload, message, lane, edge)
                    .await
            }
            ReplayWindow::Link => {
                self.logical
                    .admit_link_transaction(&self.link_windows, payload, message, lane, edge)
                    .await
            }
        }
    }

    /// The instant every link step is judged at: the inbound clock, so a test can drive lease
    /// and delegation expiry deterministically.
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

    /// The receiving end of this connection's delegation references.
    ///
    /// Lock law: held for one pure step and never across a suspension point; a poisoned lock
    /// still guards a well-formed state, since no step panics between two writes.
    pub(super) fn session_link(&self) -> std::sync::MutexGuard<'_, DelegationLessor> {
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

/// Verify one resolved frame of `wire_bytes` bytes and decode the message it carries, refusing
/// a carrier whose recorded path does not match the message.
///
/// The payload is self-contained by now: whether a delegation slot travelled inline or by
/// reference, this is the verification it always was.
pub(super) fn prepare_resolved_frame(
    network_id: u32,
    peer: Option<Did>,
    payload: MessagePayload,
    wire_bytes: usize,
    authorization: PerSlot<DelegatorAuthorization>,
) -> crate::error::Result<PreparedInboundFrame> {
    if !payload.verify_transaction_and_payload_under(network_id, authorization) {
        log_inbound_verification_failure(peer, &payload, wire_bytes);
        return Err(crate::error::Error::InvalidMessage(
            "message verification failed or message expired".to_string(),
        ));
    }
    let message = payload.transaction.data::<Message>()?;
    let kind = MessageKind::from_message(&message);
    // A path is carried exactly by connect signaling, and a carried path names no node twice
    // (#947); anything else is refused before any handler sees it.
    payload
        .relay
        .validate_signaling_path(kind.carries_signaling_path())?;
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
    prepare_resolved_frame(
        network_id,
        None,
        payload,
        bytes.len(),
        PerSlot::full_verification(),
    )
    .map(|prepared| prepared.lane)
}
