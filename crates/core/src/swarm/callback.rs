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
use crate::swarm::session_link::ReferencedDelegations;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;

mod inbound;
mod inner;
mod link_stage;
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

/// The callback a swarm delivers to until the application sets its own: every hook is a no-op.
pub(crate) struct DefaultCallback;
impl SwarmCallback for DefaultCallback {}

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
pub type SharedSwarmCallback = Arc<rings_runtime::maybe_send_sync!(dyn SwarmCallback)>;

/// Used to notify the application of events that occur in the swarm.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SwarmEvent {
    /// Indicates that the connection state of a peer has changed.
    ///
    /// `Connected` is emitted at most once per admission, when the peer's data channel is open
    /// and the peer has joined the local DHT; it is therefore the application-level fact
    /// "peer admitted". Terminal states are reported only for the admitted generation, and
    /// then after its [`SwarmEvent::PeerRetired`]; a generation the swarm retired for its own
    /// reasons reports no terminal state at all.
    ConnectionStateChange {
        /// The did of remote peer.
        peer: Did,
        /// The final state of the connection.
        state: WebrtcConnectionState,
    },
    /// An admitted peer's connection record was retired and its transport is being closed: the
    /// peer left the local DHT. Emitted from the one retirement transition, whatever reached it
    /// — remote terminal state, data-channel close, liveness or stabilization removal, capacity
    /// eviction, explicit disconnect — so the application observes the logical fact regardless
    /// of the physical event or local decision behind it. Law: for every retired generation,
    /// `Connected` started ⟺ `PeerRetired` started, with `start(Connected) <
    /// start(PeerRetired)` under the peer's ordered delivery (see
    /// [`SwarmCallback::on_event`]); the law is over starts, since a callback releases its
    /// turn at its first poll. A topology prune that keeps the record (a `Disconnected`
    /// transport allowed to recover) emits nothing. `PeerRetired` resolves the callback set at
    /// delivery time, while `ConnectionStateChange` goes to the callback set when the connection
    /// was created; the law is stated per delivery, so replacing the callback between an
    /// admission and its retirement splits the pair across the two callbacks.
    PeerRetired {
        /// The did of the retired peer.
        peer: Did,
    },
}

/// The two halves of the retirement law, as an application reads them off the event stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerTransition {
    /// The peer was admitted: `ConnectionStateChange { Connected }` started.
    Admitted,
    /// The peer's admitted record was retired: [`SwarmEvent::PeerRetired`] started.
    Retired,
}

impl SwarmEvent {
    /// The logical transition this event carries, if any: `Connected` is the physical state
    /// whose delivery is the fact "peer admitted", so its interpretation lives here, beside
    /// the retirement it is paired with. Law: for every retired generation, as seen by a
    /// callback held across it, the stream carries at most one `Admitted` and, iff it did,
    /// exactly one later `Retired` for that peer.
    pub fn peer_transition(&self) -> Option<(Did, PeerTransition)> {
        match *self {
            Self::ConnectionStateChange {
                peer,
                state: WebrtcConnectionState::Connected,
            } => Some((peer, PeerTransition::Admitted)),
            Self::PeerRetired { peer } => Some((peer, PeerTransition::Retired)),
            Self::ConnectionStateChange { .. } => None,
        }
    }
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
    /// per-peer inbound capacity. With the session hold below, an unadmitted peer occupies at
    /// most one and a half of an admitted peer's frame budgets, and a quarter of the transport's
    /// per-peer frames stays free for the control frames that release either hold.
    pre_admission: Arc<Mutex<PreAdmissionHold<HeldInboundFrame>>>,
    /// The receiving end of this connection's delegation references: the sessions the peer has
    /// announced on it and the frames waiting for one. It lives and dies with the connection;
    /// its hold is bounded by [`SESSION_HOLD_CAPACITY`] frames and by
    /// [`SESSION_HOLD_TIMEOUT`](crate::swarm::transport::SESSION_HOLD_TIMEOUT), swept by the
    /// inbound actor's periodic cleanup.
    session_link: Arc<Mutex<ReferencedDelegations<InboundFrameLease>>>,
}

/// What the transport handed over with one frame and takes back when the frame is done: the
/// raw bytes, for their length and their memory accounting, and the transport capacity they
/// occupy until the inbound actor releases it.
pub(super) struct InboundFrameLease {
    bytes: Bytes,
    transport_capacity: Option<InboundFrameCapacityLease>,
}

/// One verified frame waiting for admission, with everything its delivery needs.
///
/// The peer's authentication is not kept: it is judged as of delivery, when the handshake it
/// was waiting for has been admitted.
struct HeldInboundFrame {
    peer: Did,
    prepared: PreparedInboundFrame,
    lease: InboundFrameLease,
}

/// Frames one connection may hold for a session the peer has not backed yet: half the
/// pre-admission hold, so both holds together leave a quarter of the transport's per-peer
/// frames for the link-control frames that release them.
pub(super) const SESSION_HOLD_CAPACITY: usize = inbound::peer_capacity() / 2;

/// Where a verified frame comes from, which decides how far its delivery is waited for and
/// what its learning does.
///
/// A frame the transport just handed over is awaited to its logical completion, so the
/// transport's read loop paces this end, and what it teaches the link releases the held
/// frames that awaited it. A frame released from a hold is waited for only until the inbound
/// actor owns it, so releasing many frames at once does not stall the read loop behind each
/// one's handlers, and what it teaches is confirmed but releases nothing itself: the release
/// that freed it re-scans the hold.
#[derive(Clone, Copy, Debug)]
pub(super) enum FrameProvenance {
    /// Handed over by the transport just now.
    Arrived,
    /// Left the pre-admission hold or the session hold.
    Released,
}

impl FrameProvenance {
    /// Whether what the frame teaches the link releases held frames: an arrived frame's
    /// learning does; a released frame's learning is applied by the release that freed it,
    /// which re-scans the hold.
    pub(super) const fn releases_what_it_teaches(self) -> bool {
        match self {
            Self::Arrived => true,
            Self::Released => false,
        }
    }
}

/// How the pending handshake bound to a callback disposes of a frame from `peer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InboundGate {
    /// No handshake is pending, or it has been admitted: the frame may be delivered.
    Admitted,
    /// The handshake is with this peer and not yet admitted: the frame is early, not wrong.
    Unadmitted,
    /// The handshake is with this peer but its generation is no longer the peer's: the frame
    /// is late, and there is nothing to hold it for.
    Superseded,
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
