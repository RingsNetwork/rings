#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use async_trait::async_trait;
use bytes::Bytes;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::core::callback::InboundFrameCapacityLease;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::chunk::MessageReassembler;
use crate::dht::Did;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

mod inbound;
mod inner;
mod logical;
mod pre_admission;
mod processor;

pub(crate) use inbound::InboundCapacity;
pub(crate) use inbound::InboundLane;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use processor::prepare_transport_frame_lane_for_test;

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
    inbound::peer_capacity()
}

use inbound::InboundMailbox;
use inbound::ReassemblyClock;
use pre_admission::PreAdmissionHold;

/// The application the swarm currently delivers to, replaceable through `Swarm::set_callback`;
/// every delivery resolves it at delivery time.
///
/// Lock law: the slot is read for the duration of one clone and written for one replacement, so
/// a poisoned slot (`Error::LockPoisoned`) is the only failure either can report.
#[derive(Clone)]
pub(crate) struct SwarmCallbackSlot(Arc<RwLock<SharedSwarmCallback>>);

impl SwarmCallbackSlot {
    /// A slot holding `callback`.
    pub(crate) fn new(callback: SharedSwarmCallback) -> Self {
        Self(Arc::new(RwLock::new(callback)))
    }

    /// The callback currently set.
    pub(crate) fn current(&self) -> crate::error::Result<SharedSwarmCallback> {
        Ok(self
            .0
            .read()
            .map_err(|_| crate::error::Error::LockPoisoned)?
            .clone())
    }

    /// Replace the callback for every later delivery.
    pub(crate) fn replace(&self, callback: SharedSwarmCallback) -> crate::error::Result<()> {
        *self
            .0
            .write()
            .map_err(|_| crate::error::Error::LockPoisoned)? = callback;
        Ok(())
    }
}

/// Delivery of payloads addressed to this node that did not arrive over a connection (messages
/// drained from this node's relay inbox), through the logical stage of the inbound pipeline:
/// application validation under the inbound deadline, handler dispatch, then `on_inbound` under
/// the inbound deadline. A drained payload is already whole and admitted, so it needs nothing
/// of the connection stage (reassembly, admission gates).
pub(crate) struct LocalDelivery {
    pipeline: LogicalInbound,
}

pub use crate::error::CallbackError;
type TransportCallbackError = Box<dyn std::error::Error>;

fn into_transport_callback_error(error: CallbackError) -> TransportCallbackError {
    error
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

/// The logical stage of the inbound pipeline, independent of any connection: application
/// validation, handler dispatch, and `on_inbound`. An [`InboundProcessor`] runs it once a
/// connection's frames are reassembled and admitted; [`LocalDelivery`] runs it alone.
#[derive(Clone)]
pub(super) struct LogicalInbound {
    transport: Arc<SwarmTransport>,
    message_handler: MessageHandler,
    callback: SharedSwarmCallback,
}

/// The connection stage of the inbound pipeline for one connection: reassembly and admission
/// gates in front of the logical stage.
#[derive(Clone)]
pub(super) struct InboundProcessor {
    logical: LogicalInbound,
    reassembler: Arc<FuturesMutex<MessageReassembler>>,
    reassembly_clock: ReassemblyClock,
    pending_attempt: Arc<Mutex<Option<PendingConnectionAttempt>>>,
    /// Verified frames that arrived before this end admitted the connection; bounded by the
    /// per-peer inbound capacity, so an unadmitted peer holds no more than an admitted one.
    pre_admission: Arc<Mutex<PreAdmissionHold<HeldInboundFrame>>>,
}

/// One verified frame waiting for admission, with everything its delivery needs.
///
/// The peer's authentication is not kept: it is judged as of delivery, when the handshake it
/// was waiting for has been admitted.
struct HeldInboundFrame {
    peer: Did,
    bytes: Bytes,
    prepared: PreparedInboundFrame,
    transport_capacity: Option<InboundFrameCapacityLease>,
}

/// How the pending handshake bound to a callback disposes of a frame from `peer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InboundGate {
    /// No handshake is pending, or it has been admitted: the frame may be delivered.
    Admitted,
    /// The handshake is with this peer and not yet admitted: the frame is early, not wrong.
    Unadmitted,
    /// The frame does not belong to the pending handshake: it is refused.
    Refused,
}

/// [InnerSwarmCallback] wraps [SharedSwarmCallback] with inner handling for a specific connection.
pub struct InnerSwarmCallback {
    processor: InboundProcessor,
    inbound: InboundMailbox,
}

pub(super) struct PreparedInboundFrame {
    payload: MessagePayload,
    message: Message,
    kind: MessageKind,
    lane: InboundLane,
}
