#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use bytes::Bytes;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::core::callback::InboundCreditLease;
use rings_transport::core::pool::ChannelLane;

use crate::chunk::MessageReassembler;
use crate::dht::Did;
use crate::message::LinkWindows;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageKind;
use crate::message::MessagePayload;
use crate::swarm::session_link::DelegationLessor;
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

/// The logical stage as a connection keeps it: through its transport, held weakly.
///
/// Release law (#1093): nothing a connection owns holds its swarm transport strongly. The
/// transport owns its connections, and each connection owns its callback and, through the
/// callback's mailbox, its inbound actor; a strong reference from either would close a cycle
/// that keeps a dropped swarm alive with everything it holds, its stores included. So the
/// callback and the actor keep the logical stage weakly and bind it for one event at a time:
///
/// ```text
///   ─▶ strong   ┈▷ weak
///
///   SwarmTransport ─▶ connection ─▶ callback ─▶ mailbox ─▶ inbound actor
///         ▲                             ┊                        ┊
///         └┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┴┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┘
///                  both through InboundProcessor<WeakLogical>
///
///   one event:   InboundProcessor<WeakLogical> ─upgrade─▶ InboundProcessor<LogicalInbound>
///                                                          ─▶ SwarmTransport, for its span
/// ```
///
/// An upgrade yields the stage while the transport lives, and `None` once it is gone, which it
/// then stays: an event that arrives after the transport is gone has nothing to be delivered to.
#[derive(Clone)]
pub(crate) struct WeakLogical(Weak<SwarmTransport>);

/// The connection stage of the inbound pipeline for one connection: reassembly and admission
/// gates in front of the logical stage, reached through `L`.
///
/// What a connection keeps reaches it weakly (`InboundProcessor<WeakLogical>`, the release
/// law of [`WeakLogical`]); handling one event binds it (`InboundProcessor<LogicalInbound>`,
/// the default, which every stage that acts on a frame takes). Both forms share one set of
/// per-connection state, so binding changes only how the logical stage is reached.
#[derive(Clone)]
pub(super) struct InboundProcessor<L = LogicalInbound> {
    logical: L,
    /// The connection's reassembler, whose open messages hold the mailbox charges their first
    /// chunks took (#932).
    reassembler: Arc<FuturesMutex<MessageReassembler<Option<inbound::HeldCharge>>>>,
    reassembly_clock: ReassemblyClock,
    pending_attempt: Arc<Mutex<Option<PendingConnectionAttempt>>>,
    /// Verified frames that arrived before this end admitted the connection; bounded by the
    /// per-peer inbound capacity (32 frames). A held frame keeps its transport credit, so this
    /// hold occupies at most half of the raw frames the transport admits from one peer
    /// ([`INBOUND_PEER_FRAME_CAPACITY`](rings_transport::callback::INBOUND_PEER_FRAME_CAPACITY)).
    pre_admission: Arc<Mutex<PreAdmissionHold<HeldInboundFrame>>>,
    /// The receiving end of this connection's delegation references: the sessions the peer
    /// carried inline on it, each under a lease this end granted. A lease that ended stays
    /// resident until its slot is needed for a new one (the residence law of
    /// `swarm::session_link`). It lives and dies with the connection, and holds no frame.
    session_link: Arc<Mutex<DelegationLessor>>,
    /// The volatile replay windows of this connection's link-local transactions (#1081). They
    /// live and die with the connection, so each window spans one link generation and is
    /// released with it (durable-records, Law Window by classification).
    link_windows: Arc<FuturesMutex<LinkWindows>>,
}

/// What the transport handed over with one frame and takes back when the frame is done: the
/// raw bytes, for their length and their memory accounting, and the transport capacity they
/// occupy until the inbound actor releases it.
pub(super) struct InboundFrameLease {
    bytes: Bytes,
    transport_credit: Option<InboundCreditLease>,
    /// The transport lane the frame arrived on, whose credit it took.
    carrier: Option<ChannelLane>,
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

/// Where a verified frame comes from on its way past the admission gate, which decides how far
/// its delivery is waited for.
///
/// A frame the transport just handed over is awaited to its logical completion, so the
/// transport lane that carried it paces this end: the lane's drainer hands over the lane's
/// next frame only after this one (the transport's read loop itself never waits on core). A
/// frame released from the pre-admission hold is waited for only until the inbound actor owns
/// it, so draining the hold does not stall the draining task behind each one's handlers.
#[derive(Clone, Copy, Debug)]
pub(super) enum FrameProvenance {
    /// Handed over by the transport just now.
    Arrived,
    /// Left the pre-admission hold.
    Released,
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

/// The transport callback of one connection: its inbound pipeline and lifecycle handling,
/// reaching the logical stage through `L`.
///
/// The transport keeps the callback in the connection it serves, so the callback it keeps
/// reaches its transport weakly (`L = WeakLogical`, the default; the release law of
/// [`WeakLogical`]) and acts on each event as the [`BoundCallback`] for that event.
pub(crate) struct SwarmTransportCallback<L = WeakLogical> {
    processor: InboundProcessor<L>,
    inbound: InboundMailbox,
}

/// The callback of one connection bound to its transport for the span of one event.
type BoundCallback = SwarmTransportCallback<LogicalInbound>;

pub(super) struct PreparedInboundFrame {
    payload: MessagePayload,
    message: Message,
    kind: MessageKind,
    lane: InboundLane,
}
