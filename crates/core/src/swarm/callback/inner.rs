use std::str::FromStr;
use std::sync::Arc;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::sync::Mutex;

use async_trait::async_trait;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use bytes::Bytes;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use super::inbound::InboundMailbox;
use super::inbound::InboundSubmission;
use super::inbound::ReassemblyClock;
use super::pre_admission::Arrival;
use super::processor::prepare_resolved_frame;
use super::FrameProvenance;
use super::HeldInboundFrame;
use super::InboundFrameLease;
use super::InboundGate;
use super::InboundProcessor;
use super::PreparedInboundFrame;
use super::SwarmTransportCallback;
use super::TransportCallbackError;
use crate::delegation::DelegatorAuthorization;
use crate::dht::Did;
use crate::error::Error;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PerSlot;
use crate::swarm::detached::run_detached_or_inline;
use crate::swarm::session_link::expired_within_grace;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

impl SwarmTransportCallback {
    pub(super) fn pending_attempt(&self) -> Option<PendingConnectionAttempt> {
        self.processor.pending_attempt()
    }

    /// A second handle on this callback's connection, for a task that runs on its own.
    pub(super) fn detached_handle(&self) -> Self {
        Self {
            processor: self.processor.clone(),
            inbound: self.inbound.clone(),
        }
    }

    /// Create the transport callback of one connection of `transport`.
    pub fn new(transport: Arc<SwarmTransport>) -> Self {
        Self::new_with_reassembly_clock(transport, ReassemblyClock::system())
    }

    fn new_with_reassembly_clock(
        transport: Arc<SwarmTransport>,
        reassembly_clock: ReassemblyClock,
    ) -> Self {
        let inbound_capacity = transport.inbound_capacity();
        let processor = InboundProcessor::new(transport, reassembly_clock.clone());
        let inbound = InboundMailbox::spawn(processor.clone(), inbound_capacity, reassembly_clock);
        Self { processor, inbound }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    /// Construct an inbound actor whose chunk admission and periodic cleanup
    /// read one injected clock.
    pub(crate) fn new_with_reassembly_clock_for_test(
        transport: Arc<SwarmTransport>,
        now_ms: Arc<Mutex<u128>>,
    ) -> Self {
        Self::new_with_reassembly_clock(transport, ReassemblyClock::controlled(now_ms))
    }

    /// Bind this callback to the pending handshake that created its transport.
    pub(crate) fn with_pending_connection_attempt(
        self,
        pending_attempt: PendingConnectionAttempt,
    ) -> Self {
        self.processor.set_pending_attempt(pending_attempt);
        self
    }

    /// The frames held until their connection is admitted.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn pre_admission_held_count_for_test(&self) -> usize {
        self.processor.pre_admission().len()
    }

    /// Test builds: the inbound mailbox, whose probes witness admission, waiting, hand-off and
    /// reassembly cleanup, and whose holds pin its capacity.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn inbound_for_test(&self) -> &InboundMailbox {
        &self.inbound
    }

    /// Admit the pending connection to `did` once its data channel is open: the DHT admission,
    /// then its liveness record, then the release of what was held before admission.
    ///
    /// Pre: the caller's `connection_event_peer` found no pending attempt bound to another
    /// peer, so a bound attempt is with `did`.
    async fn admit_pending_connection(&self, did: Did) -> crate::error::Result<()> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(());
        };
        if !self
            .processor
            .logical
            .transport
            .begin_ready_connection_admission(attempt)?
        {
            return Ok(());
        }

        match self
            .processor
            .logical
            .message_handler
            .admit_dht_attempt(attempt)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => {
                if let Err(cleanup_error) = self
                    .processor
                    .logical
                    .transport
                    .cancel_unadmitted_connection(attempt)
                    .await
                {
                    tracing::warn!(
                        peer = %did,
                        generation = attempt.generation(),
                        error = ?cleanup_error,
                        "failed to close connection after admission error"
                    );
                }
                return Err(error);
            }
        }

        self.processor
            .logical
            .transport
            .record_peer_connected(attempt)
            .await;
        if self
            .processor
            .logical
            .transport
            .is_active_connection_attempt(attempt)
        {
            self.start_pre_admission_drain().await;
        }
        Ok(())
    }

    /// The peer a connection event on `cid` concerns, if the event is this callback's to act
    /// on: `None` for an unparsable identifier, for an event about another peer than the bound
    /// attempt's (whose unadmitted attempt is cancelled), and for an event about this node.
    async fn connection_event_peer(
        &self,
        cid: &str,
        operation: &str,
    ) -> crate::error::Result<Option<Did>> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("{operation}: unparsable peer {cid}");
            return Ok(None);
        };
        if self
            .cancel_mismatched_pending_connection(did, operation)
            .await?
            || self.is_local_did_event(did, operation)
        {
            return Ok(None);
        }
        Ok(Some(did))
    }

    /// Tear down the connection to `did` on a terminal `operation`: a pending handshake is
    /// discarded without touching the DHT, an active peer leaves it.
    async fn retire_on_terminal(&self, did: Did, operation: &str) -> crate::error::Result<()> {
        if self.handle_pending_terminal_event(did, operation).await? {
            return Ok(());
        }
        let Some(attempt) = self.pending_attempt() else {
            tracing::warn!("ignoring unbound {operation} for {did}");
            return Ok(());
        };
        if !self
            .processor
            .logical
            .transport
            .is_active_connection_attempt(attempt)
        {
            return Ok(());
        }
        self.processor
            .logical
            .transport
            .record_peer_disconnected(attempt)
            .await;
        self.processor
            .logical
            .message_handler
            .leave_dht_attempt(attempt)
            .await
    }

    fn pending_disconnected_before_admission(&self, did: Did) -> bool {
        let Some(attempt) = self.pending_attempt() else {
            return false;
        };
        attempt.is_with(did)
            && !self
                .processor
                .logical
                .transport
                .is_active_connection_attempt(attempt)
    }

    fn is_local_did_event(&self, did: Did, operation: &str) -> bool {
        if did != self.processor.logical.transport.dht.did {
            return false;
        }
        tracing::warn!("ignoring {operation} for local DID {did}");
        true
    }

    async fn cancel_mismatched_pending_connection(
        &self,
        did: Did,
        operation: &str,
    ) -> crate::error::Result<bool> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(false);
        };
        if attempt.is_with(did) {
            return Ok(false);
        }
        tracing::warn!(
            "ignoring {operation} for {did}; pending attempt belongs to {}",
            attempt.peer()
        );
        if self
            .processor
            .logical
            .transport
            .cancel_unadmitted_connection(attempt)
            .await?
        {
            self.processor.discard_pre_admission_hold();
            self.processor
                .logical
                .transport
                .record_peer_disconnected(attempt)
                .await;
        }
        Ok(true)
    }

    async fn handle_pending_terminal_event(
        &self,
        did: Did,
        operation: &str,
    ) -> crate::error::Result<bool> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(false);
        };
        if self
            .processor
            .logical
            .transport
            .cancel_unadmitted_connection(attempt)
            .await?
        {
            self.processor.discard_pre_admission_hold();
            self.processor
                .logical
                .transport
                .record_peer_disconnected(attempt)
                .await;
            return Ok(true);
        }
        if self
            .processor
            .logical
            .transport
            .is_active_connection_attempt(attempt)
        {
            return Ok(false);
        }
        tracing::debug!(
            "ignoring late {operation} for {did}; pending attempt belongs to generation already superseded"
        );
        Ok(true)
    }
}

impl SwarmTransportCallback {
    /// Verify one self-contained payload of `wire_len` bytes from `peer`, charging a failure
    /// to `peer`, except a frame whose only defect is an origin delegation that expired in
    /// flight, which is dropped uncharged. Post: the frame, ready for the admission gate, or
    /// `None` if it was dropped; nothing about the link is touched, so the link stage decides
    /// what, if anything, the frame teaches it.
    pub(super) async fn verify_frame(
        &self,
        peer: Option<Did>,
        payload: MessagePayload,
        wire_len: usize,
        authorization: PerSlot<DelegatorAuthorization>,
    ) -> Result<Option<PreparedInboundFrame>, TransportCallbackError> {
        let network_id = self.processor.logical.transport.network_id;
        // Law (expiry in flight): the hop answers only for its own delegation, so a frame whose
        // only defect is an origin delegation that expired in flight is not charged to it.
        if origin_expired_in_flight(network_id, &payload, self.processor.now_ms()) {
            tracing::debug!(
                peer = ?peer,
                "dropping message; its origin delegation expired in flight"
            );
            return Ok(None);
        }
        match prepare_resolved_frame(network_id, peer, payload, wire_len, authorization) {
            Ok(prepared) => Ok(Some(prepared)),
            Err(error) => {
                self.processor.record_receive_failure_now(peer).await;
                Err(error.into())
            }
        }
    }

    /// Pass one verified frame that just arrived through the pending-connection gate and the
    /// pre-admission hold to the inbound actor, waiting for its logical completion when it
    /// passes; a frame the hold keeps is released, unawaited, once the connection is admitted.
    pub(super) async fn gate_prepared_frame(
        &self,
        peer: Option<Did>,
        prepared: PreparedInboundFrame,
        lease: InboundFrameLease,
    ) -> Result<(), TransportCallbackError> {
        if !prepared.lane.admits_carrier(lease.carrier) {
            self.processor.record_receive_failure_now(peer).await;
            return Err(Error::InboundLaneMismatch.into());
        }
        let authentication = self.processor.authentication_of(peer);
        let admitted = match self.processor.pending_connection_gate(peer).await? {
            InboundGate::Admitted => true,
            InboundGate::Unadmitted => false,
            InboundGate::Superseded => {
                tracing::debug!(
                    peer = ?peer,
                    "dropping message; its connection generation is no longer the peer's"
                );
                return Ok(());
            }
            InboundGate::Refused => return Ok(()),
        };
        let Some(peer) = peer else {
            let submission = InboundSubmission::new(None, authentication, lease, prepared);
            return self
                .inbound
                .submit_prepared(&self.processor, submission)
                .await
                .map_err(Into::into);
        };
        let frame = HeldInboundFrame {
            peer,
            prepared,
            lease,
        };
        let arrival = self.processor.pre_admission().arrive(frame, admitted);
        match arrival {
            Arrival::Pass(frame) => self
                .submit_held_frame(frame, FrameProvenance::Arrived)
                .await
                .map_err(Into::into),
            Arrival::Held => {
                // The judgement and the admission commit are not one atomic step: admission may
                // have committed, and drained, between them. Re-reading admission after the frame
                // is queued closes that window, since the drain is exclusive and idempotent.
                if self.processor.pending_attempt_admitted() {
                    self.start_pre_admission_drain().await;
                } else {
                    tracing::debug!(
                        "holding message from {peer} until its pending connection is admitted"
                    );
                }
                Ok(())
            }
            Arrival::Overflow(_) => {
                tracing::debug!("dropping message from {peer}; the pre-admission hold is full");
                Ok(())
            }
        }
    }

    /// Hand one frame past the admission gate to the inbound actor, waiting as its provenance
    /// says, under the peer's authentication as of now.
    async fn submit_held_frame(
        &self,
        frame: HeldInboundFrame,
        provenance: FrameProvenance,
    ) -> crate::error::Result<()> {
        let HeldInboundFrame {
            peer,
            prepared,
            lease,
        } = frame;
        let authentication = self.processor.authentication_of(Some(peer));
        let submission = InboundSubmission::new(Some(peer), authentication, lease, prepared);
        match provenance {
            FrameProvenance::Arrived => {
                self.inbound
                    .submit_prepared(&self.processor, submission)
                    .await
            }
            FrameProvenance::Released => {
                self.inbound
                    .submit_prepared_detached(&self.processor, peer, submission)
                    .await
            }
        }
    }

    /// Start releasing held frames in arrival order to the inbound actor.
    ///
    /// The caller claims the drain synchronously so later admitted arrivals join the same ordered
    /// drain instead of racing past queued frames. The drain runs detached, or inline when no
    /// runtime is current, so the claimed queue is never left stuck (see
    /// [`run_detached_or_inline`]).
    async fn start_pre_admission_drain(&self) {
        if !self.processor.pre_admission().begin_drain() {
            return;
        }
        let drainer = self.detached_handle();
        run_detached_or_inline(Box::pin(async move {
            drainer.drain_claimed_pre_admission_hold().await;
        }))
        .await;
    }

    /// Release every frame from a claimed drain, once, to the inbound actor.
    ///
    /// A failure before actor ownership is logged and does not stop the drain: the frame was
    /// accepted from the transport when it arrived, so the admission callback must not fail on it.
    async fn drain_claimed_pre_admission_hold(&self) {
        loop {
            let Some(frame) = self.processor.pre_admission().drain_next() else {
                return;
            };
            let peer = frame.peer;
            if let Err(error) = self
                .submit_held_frame(frame, FrameProvenance::Released)
                .await
            {
                tracing::warn!(
                    peer = %peer,
                    error = ?error,
                    "failed to enqueue a message held until admission"
                );
            }
        }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn on_admitted_message_for_test(
        &self,
        cid: &str,
        msg: &[u8],
    ) -> Result<(), TransportCallbackError> {
        self.submit_inbound_message(cid, Bytes::copy_from_slice(msg), None, None)
            .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl TransportCallback for SwarmTransportCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> Result<(), TransportCallbackError> {
        let carrier = message.lane();
        let (cid, msg, transport_credit) = message.into_parts();
        self.submit_inbound_message(cid, msg, Some(transport_credit), Some(carrier))
            .await
    }

    async fn on_invalid_inbound_frame(&self, cid: &str) -> Result<(), TransportCallbackError> {
        // Test builds: the transport rejected a frame that was sent; it arrived and is done.
        #[cfg(test)]
        drop(self.processor.logical.transport.frames_for_test().arrive());
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::record_rejected_frame();
        let peer = Did::from_str(cid).ok();
        self.processor.record_receive_failure_now(peer).await;
        Ok(())
    }

    async fn on_peer_connection_state_change(
        &self,
        cid: &str,
        s: WebrtcConnectionState,
    ) -> Result<(), TransportCallbackError> {
        self.processor.logical.transport.signal_link_transition();
        let Some(did) = self
            .connection_event_peer(cid, "connection state change")
            .await?
        else {
            return Ok(());
        };

        // Admission announces `PeerAdmitted` and retirement `PeerRetired`; a physical state is
        // never the application's, so nothing here is announced as such.
        match s {
            // Peer-state progress may complete admission, but only when the
            // product snapshot also observes an open data channel. This makes
            // either browser callback order converge on the same transition.
            WebrtcConnectionState::Connecting | WebrtcConnectionState::Connected => {
                self.admit_pending_connection(did).await?;
            }
            // `Failed` and `Closed` are terminal states. Pending handshakes are
            // discarded without touching the DHT; active peers leave it.
            WebrtcConnectionState::Failed | WebrtcConnectionState::Closed => {
                self.retire_on_terminal(did, "connection terminal state")
                    .await?;
            }
            // `Disconnected` is a transient ICE state that frequently recovers
            // back to `Connected` on its own (e.g. a brief network blip or ICE
            // consent refresh). Tearing the connection down here would kill a
            // link that WebRTC could have healed, and drop the peer from the DHT
            // with no reconnect path. We leave it alone: it will either recover,
            // or degrade to `Failed`, which is handled above.
            WebrtcConnectionState::Disconnected => {
                if self.pending_disconnected_before_admission(did) {
                    tracing::debug!(
                        "ignoring pre-admission disconnected state for pending connection {did}"
                    );
                    return Ok(());
                }
                let Some(attempt) = self.pending_attempt() else {
                    tracing::warn!("ignoring unbound disconnected connection event for {did}");
                    return Ok(());
                };
                self.processor
                    .logical
                    .transport
                    .record_peer_disconnected(attempt)
                    .await;
                tracing::debug!("Connection to {did} is disconnected, waiting for recovery");
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_data_channel_open(&self, cid: &str) -> Result<(), TransportCallbackError> {
        self.processor.logical.transport.signal_link_transition();
        let Some(did) = self.connection_event_peer(cid, "data-channel open").await? else {
            return Ok(());
        };
        self.admit_pending_connection(did).await?;
        if self
            .processor
            .logical
            .transport
            .active_attempt(did)?
            .is_none()
        {
            tracing::debug!("ignoring late data-channel open for {did}");
        }
        Ok(())
    }

    async fn on_data_channel_close(&self, cid: &str) -> Result<(), TransportCallbackError> {
        let Some(did) = self
            .connection_event_peer(cid, "data-channel close")
            .await?
        else {
            return Ok(());
        };

        // The data channel closing is a reliable signal that the peer is gone
        // (e.g. it closed the connection), so tear the connection down now
        // instead of waiting for the ICE state to reach `Failed`. This is the
        // graceful counterpart to a local `disconnect()`: the remote learns of
        // it promptly without relying on the transient `Disconnected` state.
        self.retire_on_terminal(did, "data-channel close").await?;
        Ok(())
    }
}

/// Whether `payload` fails verification at `now_ms` only because its origin delegation, not
/// the hop's own, expired in flight: at most `G` ago (the expiry law of `swarm::session_link`),
/// with the hop's proof valid now and the origin's proof valid when its delegation expired.
fn origin_expired_in_flight(network_id: u32, payload: &MessagePayload, now_ms: u128) -> bool {
    let delegations = payload.delegations();
    let expires_at_ms = delegations.origin.expires_at_ms();
    !delegations.is_diagonal()
        && expired_within_grace(expires_at_ms, now_ms)
        && MessageVerificationExt::verify_at(payload, network_id, now_ms)
        && payload.transaction.verify_at(network_id, expires_at_ms)
}
