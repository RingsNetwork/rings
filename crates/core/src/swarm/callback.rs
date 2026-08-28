#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::callback::InboundFrameCapacityLease;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::chunk::MessageReassembler;
use crate::chunk::ReassemblyOutcome;
use crate::dht::Did;
use crate::message::with_message_variants;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::swarm::transport::ConnectionEventDisposition;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

mod inbound;
pub(crate) use inbound::InboundCapacity;
pub(crate) use inbound::InboundLane;

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) const fn inbound_mailbox_capacity_for_test() -> usize {
    inbound::capacity_for_test()
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) const fn inbound_application_capacity_for_test() -> usize {
    inbound::application_capacity_for_test()
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) const fn inbound_peer_capacity_for_test() -> usize {
    inbound::peer_capacity_for_test()
}
use inbound::InboundMailbox;
use inbound::ReassemblyCleanupClock;

pub use crate::error::CallbackError;
type TransportCallbackError = Box<dyn std::error::Error>;

fn into_transport_callback_error(error: CallbackError) -> TransportCallbackError {
    error
}

fn log_inbound_verification_failure(
    peer: Option<Did>,
    payload: &MessagePayload,
    wire_bytes: usize,
) {
    let message_kind = MessageKind::from_wire(&payload.transaction.data)
        .ok()
        .map(MessageKind::as_str);
    tracing::error!(
        peer = ?peer,
        tx_id = %payload.transaction.tx_id,
        destination = %payload.transaction.destination,
        message_kind,
        data_bytes = payload.transaction.data.len(),
        wire_bytes,
        "inbound message verification failed or expired"
    );
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
thread_local! {
    static ON_MESSAGE_RECURSION_DEPTH: Cell<usize> = const { Cell::new(0) };
    static MAX_ON_MESSAGE_RECURSION_DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
struct OnMessageRecursionDepthGuard;

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl OnMessageRecursionDepthGuard {
    fn enter() -> Self {
        ON_MESSAGE_RECURSION_DEPTH.with(|depth| {
            let current = depth.get().saturating_add(1);
            depth.set(current);
            MAX_ON_MESSAGE_RECURSION_DEPTH.with(|max_depth| {
                max_depth.set(max_depth.get().max(current));
            });
        });
        Self
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl Drop for OnMessageRecursionDepthGuard {
    fn drop(&mut self) {
        ON_MESSAGE_RECURSION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn reset_on_message_recursion_depth_for_test() {
    ON_MESSAGE_RECURSION_DEPTH.with(|depth| depth.set(0));
    MAX_ON_MESSAGE_RECURSION_DEPTH.with(|depth| depth.set(0));
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn max_on_message_recursion_depth_for_test() -> usize {
    MAX_ON_MESSAGE_RECURSION_DEPTH.with(Cell::get)
}

/// The [InnerSwarmCallback] will accept shared [SwarmCallback] trait object.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type SharedSwarmCallback = Arc<dyn SwarmCallback>;

/// The [InnerSwarmCallback] will accept shared [SwarmCallback] trait object.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type SharedSwarmCallback = Arc<dyn SwarmCallback + Send + Sync>;

/// Used to notify the application of events that occur in the swarm.
#[derive(Debug)]
#[non_exhaustive]
pub enum SwarmEvent {
    /// Indicates that the connection state of a peer has changed.
    ConnectionStateChange {
        /// The did of remote peer.
        peer: Did,
        /// The final state of the connection.
        state: WebrtcConnectionState,
    },
}

/// Any object that implements this trait can be used as a callback for the swarm.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait SwarmCallback {
    /// This method is invoked when a new message is received and before handling.
    ///
    /// The swarm enforces a deadline and cancels this future if it expires.
    /// Implementations must therefore be cancellation-safe at every suspension.
    async fn on_validate(&self, _payload: &MessagePayload) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked when a new message is received and after handling.
    /// Will not be invoked if the message is not for this node.
    ///
    /// The swarm enforces a deadline and cancels this future if it expires.
    /// Implementations must therefore be cancellation-safe at every suspension.
    async fn on_inbound(&self, _payload: &MessagePayload) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked after the Swarm handling.
    ///
    /// Connection events for one peer have an **ordered-start** contract when delivered by the
    /// swarm: `start(A) < start(B)` in transport order. A callback releases that ordering turn
    /// after its first poll, so `A` and `B` may remain suspended concurrently and completion is
    /// not serialized. Events for different peers are unordered.
    ///
    /// Implementations must publish any state that later same-peer callbacks need before their
    /// first suspension point. Work after an `.await` must tolerate overlap; callers that need
    /// completion ordering should add an application-owned sequencer instead of relying on the
    /// swarm delivery turn.
    async fn on_event(&self, _event: &SwarmEvent) -> Result<(), CallbackError> {
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct InboundProcessor {
    transport: Arc<SwarmTransport>,
    message_handler: MessageHandler,
    callback: SharedSwarmCallback,
    reassembler: Arc<FuturesMutex<MessageReassembler>>,
    pending_attempt: Arc<Mutex<Option<PendingConnectionAttempt>>>,
}

/// [InnerSwarmCallback] wraps [SharedSwarmCallback] with inner handling for a specific connection.
pub struct InnerSwarmCallback {
    processor: InboundProcessor,
    inbound: InboundMailbox,
}

impl InboundProcessor {
    fn pending_attempt(&self) -> Option<PendingConnectionAttempt> {
        *self
            .pending_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_pending_attempt(&self, attempt: PendingConnectionAttempt) {
        *self
            .pending_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attempt);
    }

    pub(super) async fn record_receive_failure(&self, peer: Option<Did>) {
        if let Some(peer) = peer {
            self.transport
                .record_peer_message_receive_failed(peer)
                .await;
        }
    }
}

impl InnerSwarmCallback {
    fn pending_attempt(&self) -> Option<PendingConnectionAttempt> {
        self.processor.pending_attempt()
    }

    /// Create a new [InnerSwarmCallback] with the provided transport and callback.
    pub fn new(transport: Arc<SwarmTransport>, callback: SharedSwarmCallback) -> Self {
        Self::new_with_reassembly_cleanup_clock(
            transport,
            callback,
            ReassemblyCleanupClock::system(),
        )
    }

    fn new_with_reassembly_cleanup_clock(
        transport: Arc<SwarmTransport>,
        callback: SharedSwarmCallback,
        cleanup_clock: ReassemblyCleanupClock,
    ) -> Self {
        let inbound_capacity = transport.inbound_capacity();
        let message_handler = MessageHandler::new(transport.clone(), callback.clone());
        let reassembler = MessageReassembler::with_limits_and_budget(
            transport.reassembly_limits(),
            transport.reassembly_budget(),
        );
        let processor = InboundProcessor {
            transport,
            message_handler,
            callback,
            reassembler: Arc::new(FuturesMutex::new(reassembler)),
            pending_attempt: Arc::new(Mutex::new(None)),
        };
        let inbound = InboundMailbox::spawn(processor.clone(), inbound_capacity, cleanup_clock);
        Self { processor, inbound }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    /// Construct an inbound actor with an injected periodic-cleanup clock.
    pub(crate) fn new_with_reassembly_cleanup_clock_for_test(
        transport: Arc<SwarmTransport>,
        callback: SharedSwarmCallback,
        now_ms: Arc<Mutex<u128>>,
    ) -> Self {
        Self::new_with_reassembly_cleanup_clock(
            transport,
            callback,
            ReassemblyCleanupClock::controlled(now_ms),
        )
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
    pub(crate) fn hold_application_admission_for_test(&self) -> crate::error::Result<impl Drop> {
        self.inbound.hold_application_admission_for_test()
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
                .transport
                .cancel_pending_connection(attempt)
                .await?;
            return Ok(false);
        }
        if !self
            .processor
            .transport
            .begin_ready_connection_admission(attempt)?
        {
            return Ok(false);
        }

        match self
            .processor
            .message_handler
            .admit_dht_attempt(attempt)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) => {
                if let Err(cleanup_error) = self
                    .processor
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
            .transport
            .record_peer_connected(attempt)
            .await;
        if !self
            .processor
            .transport
            .is_admitted_connection_attempt(attempt)
        {
            return Ok(false);
        }
        self.emit_connected_event_for_attempt(did, attempt).await
    }

    async fn emit_connected_event_for_attempt(
        &self,
        did: Did,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool, CallbackError> {
        let delivery = self.processor.transport.swarm_event_delivery_lock(did);
        let result = async {
            let delivery_turn = delivery.acquire().await;
            if !self
                .processor
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
        let delivery = self.processor.transport.swarm_event_delivery_lock(did);
        let result = async {
            let delivery_turn = delivery.acquire().await;
            if let Some(attempt) = attempt {
                match self
                    .processor
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
            .poll_once_then_release(self.processor.callback.on_event(&event))
            .await
    }

    fn pending_disconnected_before_admission(&self, did: Did) -> bool {
        let Some(attempt) = self.pending_attempt() else {
            return false;
        };
        attempt.peer() == did
            && !self
                .processor
                .transport
                .is_admitted_connection_attempt(attempt)
    }

    fn is_local_did_event(&self, did: Did, operation: &str) -> bool {
        if did != self.processor.transport.dht.did {
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
            .transport
            .cancel_pending_connection(attempt)
            .await?
        {
            self.processor
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
            .transport
            .cancel_pending_connection(attempt)
            .await?
        {
            self.processor
                .transport
                .record_peer_disconnected(attempt)
                .await;
            return Ok(true);
        }
        if self
            .processor
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

impl InboundProcessor {
    pub(super) async fn pending_connection_allows_message(
        &self,
        peer: Option<Did>,
    ) -> crate::error::Result<bool> {
        let Some(attempt) = self.pending_attempt() else {
            return Ok(true);
        };
        let Some(peer) = peer else {
            tracing::warn!(
                "ignoring message from unparsable peer; pending attempt belongs to {}",
                attempt.peer()
            );
            return Ok(false);
        };
        if attempt.peer() != peer {
            tracing::warn!(
                "ignoring message from {peer}; pending attempt belongs to {}",
                attempt.peer()
            );
            self.transport.cancel_pending_connection(attempt).await?;
            return Ok(false);
        }
        if !self.transport.is_admitted_connection_attempt(attempt) {
            tracing::debug!("ignoring message from {peer}; pending connection is not admitted yet");
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) async fn handle_payload(
        &self,
        payload: &MessagePayload,
        prepared_message: Option<Message>,
    ) -> crate::error::Result<()> {
        let message = match prepared_message {
            Some(message) => message,
            None => payload.transaction.data()?,
        };

        macro_rules! dispatch_message_body {
            (Chunk, $msg:expr) => {{
                let _ = $msg;
                Err(crate::error::Error::InboundActorInvariantViolation)
            }};
            ($variant:ident, $msg:expr) => {
                self.message_handler.handle(payload, $msg).await
            };
        }
        macro_rules! dispatch_message {
            ($( $(#[$docs:meta])* $index:literal => $variant:ident($body:ty): $class:ident, $storage_route:ident ),+ $(,)?) => {
                match message {
                    $(Message::$variant(ref msg) => dispatch_message_body!($variant, msg)),+
                }
            };
        }

        let result = with_message_variants!(dispatch_message);

        // A handler that errored must not then be reported to the application as a successful
        // inbound message: surface the error and do not run `on_inbound` for it.
        if let Err(e) = result {
            tracing::error!("Failed to handle_payload: {e:?}");
            return Err(e);
        }

        Ok(())
    }

    pub(super) fn is_local_destination(&self, payload: &MessagePayload) -> bool {
        payload.transaction.destination == self.transport.dht.did
    }

    pub(super) async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), CallbackError> {
        self.callback.on_inbound(payload).await
    }

    pub(super) async fn handle_chunk(&self, chunk: crate::chunk::Chunk) -> ReassemblyOutcome {
        self.reassembler.lock().await.handle_retained_outcome(chunk)
    }

    pub(super) async fn remove_expired_reassembly_at(&self, now_ms: u128) {
        self.reassembler.lock().await.remove_expired_at(now_ms);
    }

    pub(super) async fn decode_verified_message(
        &self,
        peer: Option<Did>,
        msg: &[u8],
    ) -> crate::error::Result<MessagePayload> {
        let payload = match MessagePayload::from_wire(msg) {
            Ok(payload) => payload,
            Err(e) => {
                self.record_receive_failure(peer).await;
                return Err(e);
            }
        };
        if !(payload.verify() && payload.transaction.verify()) {
            log_inbound_verification_failure(peer, &payload, msg.len());
            self.record_receive_failure(peer).await;
            return Err(crate::error::Error::InvalidMessage(
                "message verification failed or message expired".to_string(),
            ));
        }
        let is_chunk = matches!(payload.transaction.data::<Message>()?, Message::Chunk(_));
        self.accept_preverified_message(peer, payload, !is_chunk)
            .await
    }

    pub(super) async fn accept_preverified_message(
        &self,
        peer: Option<Did>,
        payload: MessagePayload,
        record_as_logical_message: bool,
    ) -> crate::error::Result<MessagePayload> {
        if payload.is_expired() || payload.transaction.is_expired() {
            self.record_receive_failure(peer).await;
            return Err(crate::error::Error::InvalidMessage(
                "message expired after transport admission".to_string(),
            ));
        }
        if record_as_logical_message {
            let useful_bytes = u64::try_from(payload.transaction.data.len())
                .map_err(|_| crate::error::Error::MessageSizeOverflow)?;
            if let (Some(peer), Some(attempt)) = (peer, self.pending_attempt()) {
                if attempt.peer() == peer {
                    self.transport
                        .record_peer_message_received(attempt, useful_bytes)
                        .await;
                }
            }
        }
        Ok(payload)
    }
}

pub(super) struct PreparedInboundFrame {
    payload: MessagePayload,
    message: Message,
    lane: InboundLane,
}

fn prepare_transport_frame(
    peer: Option<Did>,
    bytes: &[u8],
) -> crate::error::Result<PreparedInboundFrame> {
    let payload = MessagePayload::from_wire(bytes)?;
    if !(payload.transaction.verify() && payload.verify()) {
        log_inbound_verification_failure(peer, &payload, bytes.len());
        return Err(crate::error::Error::InvalidMessage(
            "message verification failed or message expired".to_string(),
        ));
    }
    let message = payload.transaction.data::<Message>()?;
    let lane = InboundLane::from_kind(MessageKind::from_message(&message));
    Ok(PreparedInboundFrame {
        payload,
        message,
        lane,
    })
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn prepare_transport_frame_lane_for_test(
    bytes: &[u8],
) -> crate::error::Result<InboundLane> {
    prepare_transport_frame(None, bytes).map(|prepared| prepared.lane)
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
        let prepared = match prepare_transport_frame(peer, msg.as_ref()) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.processor.record_receive_failure(peer).await;
                return Err(error.into());
            }
        };
        self.inbound
            .submit_prepared(&self.processor, peer, msg, prepared, transport_capacity)
            .await
            .map_err(Into::into)
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
        self.processor
            .record_receive_failure(Did::from_str(cid).ok())
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
                    .transport
                    .is_admitted_connection_attempt(attempt)
                {
                    return Ok(());
                }
                self.processor
                    .transport
                    .record_peer_disconnected(attempt)
                    .await;
                self.processor
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
            && !self.processor.transport.is_admitted_connection(did)
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
            .transport
            .is_admitted_connection_attempt(attempt)
        {
            return Ok(());
        }
        self.processor
            .transport
            .record_peer_disconnected(attempt)
            .await;
        self.processor
            .message_handler
            .leave_dht_attempt(attempt)
            .await?;
        Ok(())
    }
}
