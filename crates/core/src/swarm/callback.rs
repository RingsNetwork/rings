#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::core::callback::InboundCreditLease;
use rings_transport::core::pool::ChannelLane;

use crate::chunk::MessageReassembler;
use crate::dht::Did;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::swarm::session_link::ReferencedDelegations;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SessionHoldPermit;
use crate::swarm::transport::SwarmTransport;

mod inbound;
mod inner;
mod link_stage;
mod logical;
mod pre_admission;
mod processor;

pub(crate) use inbound::InboundCapacity;
pub(crate) use inbound::InboundLane;
pub(crate) use logical::LocalDelivered;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use processor::prepare_transport_frame_lane_for_test;

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

/// Delivery of payloads addressed to this node that did not arrive over a connection (messages
/// drained from this node's relay inbox), through the logical stage of the inbound pipeline:
/// handler dispatch, then the application inbox if the body is application data, never waiting
/// for room. A drained payload is already whole and was verified by the inbox witness, so it
/// needs nothing of the connection stage (reassembly, the connection's gates); its transaction
/// is admitted by the delivery itself.
pub(crate) struct LocalDelivery {
    pipeline: LogicalInbound,
}

/// The error the transport's callback trait returns.
type TransportCallbackError = Box<dyn std::error::Error>;

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

/// The logical stage of the inbound pipeline, independent of any connection: handler dispatch,
/// then the application inbox for application data addressed to this node. An
/// [`InboundProcessor`] runs it once a connection's frames are reassembled and admitted;
/// [`LocalDelivery`] runs it alone.
#[derive(Clone)]
pub(super) struct LogicalInbound {
    transport: Arc<SwarmTransport>,
    message_handler: MessageHandler,
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
    /// per-peer inbound capacity (32 frames). A held frame keeps its transport credit, so this
    /// hold occupies at most half of the raw frames the transport admits from one peer
    /// ([`INBOUND_PEER_FRAME_CAPACITY`](rings_transport::callback::INBOUND_PEER_FRAME_CAPACITY));
    /// a frame in the session hold below gives its credit back and is bounded by that hold's
    /// own capacity and the node's session-hold budget instead.
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
    transport_credit: Option<InboundCreditLease>,
    /// The transport lane the frame arrived on, whose credit it took.
    carrier: Option<ChannelLane>,
    /// The frame's place in the node's session-hold budget, while the session link holds it.
    held: Option<SessionHoldPermit>,
    /// Test builds: the frame's conservation witness, released with the lease.
    #[cfg(test)]
    in_flight: crate::swarm::transport::FrameInFlight,
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
/// pre-admission hold. A held frame has given its transport credit back (see
/// `submit_inbound_message`), so the hold never occupies the lane window that carries the
/// link-control answer releasing it; the holds of every connection together are bounded by the
/// node's [`SessionHoldBudget`](crate::swarm::transport::SessionHoldBudget).
pub(super) const SESSION_HOLD_CAPACITY: usize = inbound::peer_capacity() / 2;

/// Where a verified frame comes from, which decides how far its delivery is waited for and
/// what its learning does.
///
/// A frame the transport just handed over is awaited to its logical completion, so the
/// transport lane that carried it paces this end: the lane's drainer hands over the lane's
/// next frame only after this one (the transport's read loop itself never waits on core), and
/// what the frame teaches the link releases the held frames that awaited it. A frame released
/// from a hold is waited for only until the inbound actor owns it, so releasing many frames at
/// once does not stall the releasing task behind each one's handlers, and what it teaches is
/// confirmed but releases nothing itself: the release that freed it re-scans the hold.
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

/// The transport callback of one connection: its inbound pipeline and lifecycle handling.
pub(crate) struct SwarmTransportCallback {
    processor: InboundProcessor,
    inbound: InboundMailbox,
}

pub(super) struct PreparedInboundFrame {
    payload: MessagePayload,
    message: Message,
    kind: MessageKind,
    lane: InboundLane,
}
