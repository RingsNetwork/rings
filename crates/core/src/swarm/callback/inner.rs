use std::str::FromStr;
use std::sync::Arc;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::callback::InboundFrameCapacityLease;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use super::inbound::InboundMailbox;
use super::inbound::InboundSubmission;
use super::inbound::ReassemblyClock;
use super::into_transport_callback_error;
use super::pre_admission::Arrival;
use super::processor::prepare_transport_frame;
use super::CallbackError;
use super::HeldInboundFrame;
use super::InboundGate;
use super::InboundProcessor;
use super::InnerSwarmCallback;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::SharedSwarmCallback;
use super::SwarmEvent;
use super::TransportCallbackError;
use crate::dht::Did;
use crate::measure::Authentication;
use crate::swarm::transport::ConnectionEventDisposition;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_pre_admission_drain(drainer: InnerSwarmCallback) -> bool {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    drop(runtime.spawn(async move {
        drainer.drain_claimed_pre_admission_hold().await;
    }));
    true
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_pre_admission_drain(drainer: InnerSwarmCallback) -> bool {
    wasm_bindgen_futures::spawn_local(async move {
        drainer.drain_claimed_pre_admission_hold().await;
    });
    true
}

impl InnerSwarmCallback {
    fn pending_attempt(&self) -> Option<PendingConnectionAttempt> {
        self.processor.pending_attempt()
    }

    /// Create a new [InnerSwarmCallback] with the provided transport and callback.
    pub fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        Self::new_with_reassembly_clock(transport, callback, ReassemblyClock::system())
    }

    fn new_with_reassembly_clock(
        transport: Arc<SwarmTransport>,
        callback: SharedSwarmCallback,
        reassembly_clock: ReassemblyClock,
    ) -> Self {
        let inbound_capacity = transport.inbound_capacity();
        let processor = InboundProcessor::new(transport, callback, reassembly_clock.clone());
        let inbound = InboundMailbox::spawn(processor.clone(), inbound_capacity, reassembly_clock);
        Self { processor, inbound }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    /// Construct an inbound actor whose chunk admission and periodic cleanup
    /// read one injected clock.
    pub(crate) fn new_with_reassembly_clock_for_test(
        transport: Arc<SwarmTransport>,
        callback: SharedSwarmCallback,
        now_ms: Arc<Mutex<u128>>,
    ) -> Self {
        Self::new_with_reassembly_clock(transport, callback, ReassemblyClock::controlled(now_ms))
    }

    /// Bind this callback to the pending handshake that created its transport.
    pub(crate) fn with_pending_connection_attempt(
        self,
        pending_attempt: PendingConnectionAttempt,
    ) -> Self {
        self.processor.set_pending_attempt(pending_attempt);
        self
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn inbound_admitted_count_for_test(&self) -> usize {
        self.inbound.admitted_count_for_test()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn pre_admission_held_count_for_test(&self) -> usize {
        self.processor.pre_admission().len()
    }

    /// Resolve once `predicate` holds over the admitted inbound count,
    /// re-checked on every capacity reservation, transition, or release.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_inbound_admitted_count_for_test(
        &self,
        predicate: impl Fn(usize) -> bool,
    ) {
        self.inbound.await_admitted_count_for_test(predicate).await;
    }

    /// Resolve once `predicate` holds over the number of frames whose raw
    /// transport lease this mailbox has released after core admission.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_inbound_handoffs_for_test(&self, predicate: impl Fn(u64) -> bool) {
        self.inbound.await_handoffs_for_test(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn reassembly_cleanup_passes_for_test(&self) -> u64 {
        self.inbound.cleanup_passes_for_test()
    }

    /// Resolve once `predicate` holds over the number of completed reassembly
    /// cleanup passes, periodic or close-time.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_reassembly_cleanup_passes_for_test(
        &self,
        predicate: impl Fn(u64) -> bool,
    ) {
        self.inbound.await_cleanup_passes_for_test(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn hold_application_admission_for_test(&self) -> crate::error::Result<impl Drop> {
        self.inbound.hold_application_admission_for_test()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn hold_application_capacity_for_test(
        &self,
        peer: Did,
    ) -> crate::error::Result<impl Drop> {
        self.inbound.hold_application_capacity_for_test(peer)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn close_inbound_for_test(&self) {
        self.inbound.close_for_test();
    }

    async fn admit_pending_connection(&self, did: Did) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(false);
        };
        if attempt.peer() != did {
            tracing::warn!(
                "ignoring data-channel open for {did}; pending attempt belongs to {}",
                attempt.peer()
            );
            self.processor
                .logical
                .transport
                .cancel_pending_connection(attempt)
                .await?;
            return Ok(false);
        }
        if !self
            .processor
            .logical
            .transport
            .begin_ready_connection_admission(attempt)?
        {
            return Ok(false);
        }

        match self
            .processor
            .logical
            .message_handler
            .admit_dht_attempt(attempt)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) => {
                if let Err(cleanup_error) = self
                    .processor
                    .logical
                    .transport
                    .cancel_pending_connection(attempt)
                    .await
                {
                    tracing::warn!(
                        peer = %did,
                        generation = attempt.generation(),
                        error = ?cleanup_error,
                        "failed to close connection after admission error"
                    );
                }
                return Err(error.into());
            }
        }

        self.processor
            .logical
            .transport
            .record_peer_connected(attempt)
            .await;
        if !self
            .processor
            .logical
            .transport
            .is_admitted_connection_attempt(attempt)
        {
            return Ok(false);
        }
        let connected = self.emit_connected_event_for_attempt(did, attempt).await;
        if self
            .processor
            .logical
            .transport
            .is_admitted_connection_attempt(attempt)
        {
            self.start_pre_admission_drain().await;
        }
        connected
    }

    async fn emit_connected_event_for_attempt(
        &self,
        did: Did,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool, CallbackError> {
        let delivery = self
            .processor
            .logical
            .transport
            .swarm_event_delivery_lock(did);
        let result = async {
            let delivery_turn = delivery.acquire().await;
            if !self
                .processor
                .logical
                .transport
                .is_admitted_connection_attempt(attempt)
            {
                tracing::debug!("suppressing connected event for {did}; connection was retired before event delivery");
                return Ok(false);
            }
            self.emit_connection_state_change_after_ordered_start(
                delivery_turn,
                did,
                WebrtcConnectionState::Connected,
            )
            .await?;
            Ok(true)
        }
        .await;
        self.processor
            .logical
            .transport
            .prune_swarm_event_delivery_lock(did, &delivery);
        result
    }

    async fn emit_connection_state_change(
        &self,
        did: Did,
        state: WebrtcConnectionState,
        attempt: Option<PendingConnectionAttempt>,
    ) -> Result<(), CallbackError> {
        let delivery = self
            .processor
            .logical
            .transport
            .swarm_event_delivery_lock(did);
        let result = async {
            let delivery_turn = delivery.acquire().await;
            if let Some(attempt) = attempt {
                match self
                    .processor
                    .logical
                    .transport
                    .connection_event_disposition(attempt)?
                {
                    ConnectionEventDisposition::Deliver => {}
                    ConnectionEventDisposition::Suppress { active } => {
                        tracing::debug!(
                            peer = %did,
                            generation = attempt.generation(),
                            active_generation = active.generation(),
                            state = ?state,
                            "suppressing connection event from superseded generation"
                        );
                        return Ok(());
                    }
                }
            }
            self.emit_connection_state_change_after_ordered_start(delivery_turn, did, state)
                .await
        }
        .await;
        self.processor
            .logical
            .transport
            .prune_swarm_event_delivery_lock(did, &delivery);
        result
    }

    async fn emit_connection_state_change_after_ordered_start(
        &self,
        delivery_turn: crate::swarm::transport::SwarmEventDeliveryTurn,
        did: Did,
        state: WebrtcConnectionState,
    ) -> Result<(), CallbackError> {
        let event = SwarmEvent::ConnectionStateChange { peer: did, state };
        delivery_turn
            .poll_once_then_release(self.processor.logical.callback.on_event(&event))
            .await
    }

    fn pending_disconnected_before_admission(&self, did: Did) -> bool {
        let Some(attempt) = self.pending_attempt() else {
            return false;
        };
        attempt.peer() == did
            && !self
                .processor
                .logical
                .transport
                .is_admitted_connection_attempt(attempt)
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
    ) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(false);
        };
        if attempt.peer() == did {
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
            .cancel_pending_connection(attempt)
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
    ) -> Result<bool, CallbackError> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(false);
        };
        if self
            .processor
            .logical
            .transport
            .cancel_pending_connection(attempt)
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
            .is_admitted_connection_attempt(attempt)
        {
            return Ok(false);
        }
        tracing::debug!(
            "ignoring late {operation} for {did}; pending attempt belongs to generation already superseded"
        );
        Ok(true)
    }
}

impl InnerSwarmCallback {
    async fn submit_inbound_message(
        &self,
        cid: &str,
        msg: Bytes,
        transport_capacity: Option<InboundFrameCapacityLease>,
    ) -> Result<(), TransportCallbackError> {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        let _depth_guard = OnMessageRecursionDepthGuard::enter();

        let peer = Did::from_str(cid).ok();
        let authentication = peer.map_or(Authentication::Unauthenticated, |peer| {
            self.processor.peer_authentication(peer)
        });
        let prepared = match prepare_transport_frame(
            self.processor.logical.transport.network_id,
            peer,
            msg.as_ref(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.processor
                    .record_receive_failure(peer, authentication)
                    .await;
                return Err(error.into());
            }
        };
        let admitted = match self.processor.pending_connection_gate(peer).await? {
            InboundGate::Admitted => true,
            InboundGate::Unadmitted => false,
            InboundGate::Refused => return Ok(()),
        };
        let Some(peer) = peer else {
            let submission =
                InboundSubmission::new(None, authentication, msg, prepared, transport_capacity);
            return self
                .inbound
                .submit_prepared(&self.processor, submission)
                .await
                .map_err(Into::into);
        };
        let frame = HeldInboundFrame {
            peer,
            bytes: msg,
            prepared,
            transport_capacity,
        };
        let arrival = self.processor.pre_admission().arrive(frame, admitted);
        match arrival {
            Arrival::Pass(frame) => self.deliver_held_frame(frame).await.map_err(Into::into),
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

    /// Deliver one frame past the admission gate.
    async fn deliver_held_frame(&self, frame: HeldInboundFrame) -> crate::error::Result<()> {
        let HeldInboundFrame {
            peer,
            bytes,
            prepared,
            transport_capacity,
        } = frame;
        let authentication = self.processor.peer_authentication(peer);
        let submission = InboundSubmission::new(
            Some(peer),
            authentication,
            bytes,
            prepared,
            transport_capacity,
        );
        self.inbound
            .submit_prepared(&self.processor, submission)
            .await
    }

    /// Transfer one frame past the admission gate without waiting for logical completion.
    async fn enqueue_held_frame(&self, frame: HeldInboundFrame) -> crate::error::Result<()> {
        let HeldInboundFrame {
            peer,
            bytes,
            prepared,
            transport_capacity,
        } = frame;
        let authentication = self.processor.peer_authentication(peer);
        let submission = InboundSubmission::new(
            Some(peer),
            authentication,
            bytes,
            prepared,
            transport_capacity,
        );
        self.inbound
            .submit_prepared_detached(&self.processor, peer, submission)
            .await
    }

    /// Start releasing held frames in arrival order to the inbound actor.
    ///
    /// The caller claims the drain synchronously so later admitted arrivals join the same ordered
    /// drain instead of racing past queued frames. Native and wasm runtimes run the drain in the
    /// background; an unavailable native runtime falls back to the old inline drain so the claimed
    /// queue is not left stuck.
    async fn start_pre_admission_drain(&self) {
        if !self.processor.pre_admission().begin_drain() {
            return;
        }
        let drainer = Self {
            processor: self.processor.clone(),
            inbound: self.inbound.clone(),
        };
        if !spawn_pre_admission_drain(drainer) {
            self.drain_claimed_pre_admission_hold().await;
        }
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
            if let Err(error) = self.enqueue_held_frame(frame).await {
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
        self.submit_inbound_message(cid, Bytes::copy_from_slice(msg), None)
            .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl TransportCallback for InnerSwarmCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> Result<(), TransportCallbackError> {
        let (cid, msg, transport_capacity) = message.into_parts();
        self.submit_inbound_message(cid, msg, Some(transport_capacity))
            .await
    }

    async fn on_invalid_inbound_frame(&self, cid: &str) -> Result<(), TransportCallbackError> {
        let peer = Did::from_str(cid).ok();
        let authentication = peer.map_or(Authentication::Unauthenticated, |peer| {
            self.processor.peer_authentication(peer)
        });
        self.processor
            .record_receive_failure(peer, authentication)
            .await;
        Ok(())
    }

    async fn on_peer_connection_state_change(
        &self,
        cid: &str,
        s: WebrtcConnectionState,
    ) -> Result<(), TransportCallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_peer_connection_state_change parse did failed: {}", cid);
            return Ok(());
        };
        if self
            .cancel_mismatched_pending_connection(did, "connection state change")
            .await
            .map_err(into_transport_callback_error)?
        {
            return Ok(());
        }
        if self.is_local_did_event(did, "connection state change") {
            return Ok(());
        }

        let admission_completed = match s {
            // Peer-state progress may complete admission, but only when the
            // product snapshot also observes an open data channel. This makes
            // either browser callback order converge on the same transition.
            WebrtcConnectionState::Connecting | WebrtcConnectionState::Connected => self
                .admit_pending_connection(did)
                .await
                .map_err(into_transport_callback_error)?,
            // `Failed` and `Closed` are terminal states. Pending handshakes are
            // discarded without touching the DHT; active peers leave it.
            WebrtcConnectionState::Failed | WebrtcConnectionState::Closed => {
                if self
                    .handle_pending_terminal_event(did, "connection terminal state")
                    .await
                    .map_err(into_transport_callback_error)?
                {
                    return Ok(());
                }
                let Some(attempt) = self.pending_attempt() else {
                    tracing::warn!("ignoring unbound terminal connection event for {did}");
                    return Ok(());
                };
                if !self
                    .processor
                    .logical
                    .transport
                    .is_admitted_connection_attempt(attempt)
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
                    .await?;
                false
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
                false
            }
            _ => false,
        };

        // Data-channel admission emits the application-level Connected event.
        // Other state changes are passed through directly, unless this exact
        // callback completed admission and already emitted the ordered Connected event.
        if s != WebrtcConnectionState::Connected && !admission_completed {
            self.emit_connection_state_change(did, s, self.pending_attempt())
                .await
                .map_err(into_transport_callback_error)?
        }

        Ok(())
    }

    async fn on_data_channel_open(&self, cid: &str) -> Result<(), TransportCallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_data_channel_open parse did failed: {}", cid);
            return Ok(());
        };
        if self
            .cancel_mismatched_pending_connection(did, "data-channel open")
            .await
            .map_err(into_transport_callback_error)?
        {
            return Ok(());
        }
        if self.is_local_did_event(did, "data-channel open") {
            return Ok(());
        }

        if !self
            .admit_pending_connection(did)
            .await
            .map_err(into_transport_callback_error)?
            && !self.processor.logical.transport.is_admitted_connection(did)
        {
            tracing::debug!("ignoring late data-channel open for {did}");
        }
        Ok(())
    }

    async fn on_data_channel_close(&self, cid: &str) -> Result<(), TransportCallbackError> {
        let Ok(did) = Did::from_str(cid) else {
            tracing::warn!("on_data_channel_close parse did failed: {}", cid);
            return Ok(());
        };
        if self
            .cancel_mismatched_pending_connection(did, "data-channel close")
            .await
            .map_err(into_transport_callback_error)?
        {
            return Ok(());
        }
        if self.is_local_did_event(did, "data-channel close") {
            return Ok(());
        }

        // The data channel closing is a reliable signal that the peer is gone
        // (e.g. it closed the connection), so tear the connection down now
        // instead of waiting for the ICE state to reach `Failed`. This is the
        // graceful counterpart to a local `disconnect()`: the remote learns of
        // it promptly without relying on the transient `Disconnected` state.
        if self
            .handle_pending_terminal_event(did, "data-channel close")
            .await
            .map_err(into_transport_callback_error)?
        {
            return Ok(());
        }
        let Some(attempt) = self.pending_attempt() else {
            tracing::warn!("ignoring unbound data-channel close for {did}");
            return Ok(());
        };
        if !self
            .processor
            .logical
            .transport
            .is_admitted_connection_attempt(attempt)
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
            .await?;
        Ok(())
    }
}
