//! The callback bound to one connection: admission, credit, and per-lane dispatch.
//!
//! An arriving raw frame takes one of three paths, decided synchronously so that reading a
//! channel never waits on the protocol:
//!
//! ```text
//!   raw ─decode─▶ Signal(s)  ─▶ LinkCredit::apply(s)                  (applied at once)
//!            └──▶ Custom(p)  ─admit(lane)─▶ lane FIFO ─drainer─▶ on_admitted_message
//!            └──▶ invalid    ─▶ reported to the callback
//! ```
//!
//! Law (lane order). Frames of one lane reach the protocol callback in arrival order, one at a
//! time: each lane has one FIFO and at most one drainer, which hands a frame over only after
//! the previous one was taken over. Law (no blocked credit). A credit frame is applied before
//! any frame queued ahead of it is dispatched, so a sender waiting for credit never waits on
//! this end's protocol handlers. The FIFO of a lane holds at most its credit window, since
//! every queued frame holds a [`CreditPermit`].

#[cfg(rings_transport_runtime)]
use std::collections::VecDeque;
use std::future::Future;
#[cfg(rings_transport_runtime)]
use std::mem;
#[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
use std::rc::Rc;
#[cfg(any(test, rings_transport_runtime))]
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
#[cfg(rings_transport_runtime)]
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use bytes::Bytes;

use super::inbound_frame_exceeds_protocol_ceiling;
use super::link_credit::CreditReservation;
use super::link_credit::LaneCreditReservation;
use super::link_credit::LaneCreditView;
use super::link_credit::LinkCredit;
use super::link_credit::NodeFramePool;
use super::AdmittedInboundFrame;
use super::InboundFrameAdmission;
use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::BoxedTransportCallback;
use crate::core::callback::InboundCreditLease;
#[cfg(rings_transport_runtime)]
use crate::core::credit::credit_index;
use crate::core::credit::CreditIndex;
use crate::core::credit::LaneSignal;
#[cfg(rings_transport_runtime)]
use crate::core::credit::PerLane;
use crate::core::pool::ChannelLane;
use crate::core::transport::BorrowedTransportMessage;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportConnectionState;
use crate::core::transport::TransportMessage;
use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
use crate::error::Error;
use crate::error::Result;
use crate::notifier::Notifier;
#[cfg(rings_transport_runtime)]
use crate::sync_utils::lock_recover;

/// Retry backoff between failed outbound credit-control frame transmissions.
pub const CREDIT_RESEND_PAUSE: Duration = Duration::from_secs(1);

/// RAII outbound credit-control frame dequeued from [`InnerTransportCallback::next_credit_frame`].
///
/// # Invariant (Zero Lost Cumulative Credit)
///
/// If this guard is dropped before [`Self::mark_sent`] is called (for example because the backend's
/// send failed or its pump task was cancelled), the underlying cumulative `LaneSignal` is
/// automatically requeued onto the lane's outbox so the peer's sender can never stall permanently.
#[derive(Debug)]
pub struct CreditFrame {
    wire: Bytes,
    index: CreditIndex,
    signal: Option<LaneSignal>,
    link: Weak<LinkCredit>,
}

impl CreditFrame {
    /// Borrow the wire-encoded `TransportMessage::Signal` bytes to transmit on [`Self::lane`].
    pub const fn wire(&self) -> &Bytes {
        &self.wire
    }

    /// Return the [`ChannelLane`] on which this credit control frame belongs.
    pub const fn lane(&self) -> ChannelLane {
        self.index.lane()
    }

    /// Disarm automatic requeue after the wire frame has been accepted for transmission.
    pub fn mark_sent(&mut self) {
        self.signal = None;
    }
}

impl Drop for CreditFrame {
    fn drop(&mut self) {
        if let Some(signal) = self.signal.take() {
            if let Some(link) = self.link.upgrade() {
                let _ = link.requeue(self.index, signal);
            }
        }
    }
}

/// Weakly-held periodic idle-lane shrink handle created by
/// [`InnerTransportCallback::weak_idle_checker`].
///
/// Holds only a [`Weak`] reference to the connection's credit state so periodic timer loops never
/// keep retired connections or callbacks alive.
#[derive(Clone, Debug)]
pub struct IdleLaneChecker {
    link: Weak<LinkCredit>,
}

impl IdleLaneChecker {
    /// Run one idle-lane shrink pass (`Law 3`), returning `false` once the connection's credit
    /// state has been closed or dropped.
    pub fn check_once(&self) -> bool {
        self.link.upgrade().is_some_and(|link| link.idle_check())
    }
}

/// The frames of one lane admitted and not yet handed to the protocol callback.
#[cfg(rings_transport_runtime)]
#[derive(Default)]
struct LaneQueue {
    /// Admitted frames in arrival order.
    frames: VecDeque<AdmittedInboundFrame>,
    /// Whether a drainer owns the queue.
    draining: bool,
}

/// Queued peer-connection state transitions for one connection, drained in arrival order off the
/// adapter callback so reentrant closes never deadlock and rapid transitions (`Connected` then
/// `Closed`) never overtake each other.
#[cfg(rings_transport_runtime)]
#[derive(Default)]
struct PeerStateQueue {
    /// State transitions in arrival order.
    states: VecDeque<TransportConnectionState>,
    /// Whether a drainer task currently owns the queue.
    draining: bool,
}

/// Wraps a transport callback with handling bound to one connection.
pub struct InnerTransportCallback {
    pub(super) cid: Arc<str>,
    pub(super) callback: BoxedTransportCallback,
    data_channel_state_notifier: Notifier,
    /// Per-lane credit flow control of this connection.
    link_credit: Arc<LinkCredit>,
    /// The per-lane FIFOs between admission and the protocol callback.
    #[cfg(rings_transport_runtime)]
    lanes: Mutex<PerLane<LaneQueue>>,
    /// Arrival-ordered FIFO of peer-connection state transitions.
    #[cfg(rings_transport_runtime)]
    peer_states: Mutex<PeerStateQueue>,
    #[cfg(any(test, rings_transport_runtime))]
    pub(super) invalid_frame_report_state: AtomicUsize,
}

#[cfg(rings_transport_runtime)]
macro_rules! define_shared_inbound_frame_paths {
    ($shared:ident) => {
        /// Decode, credit-admit, and report one raw frame from a transport adapter.
        ///
        /// Rejected malformed, oversized and over-credit frames are reported to the callback; a
        /// credit frame is applied and yields nothing to dispatch.
        pub fn prepare_inbound_frame(
            self: &$shared<Self>,
            raw: Bytes,
            lane: ChannelLane,
        ) -> Option<AdmittedInboundFrame> {
            let received_bytes = raw.len();
            match self.admit_inbound_frame(raw, lane) {
                InboundFrameAdmission::Admitted(frame) => Some(frame),
                InboundFrameAdmission::Credit => None,
                InboundFrameAdmission::Malformed(error) => {
                    tracing::warn!(
                        peer = %self.cid,
                        bytes = received_bytes,
                        %error,
                        "rejected malformed data-channel message"
                    );
                    self.report_invalid_inbound_frame();
                    None
                }
                InboundFrameAdmission::Oversized { bytes, max_bytes } => {
                    tracing::warn!(
                        peer = %self.cid,
                        bytes,
                        max_bytes,
                        "rejected oversized data-channel message before dispatch"
                    );
                    self.report_invalid_inbound_frame();
                    None
                }
                InboundFrameAdmission::CreditExceeded {
                    received,
                    advertised,
                } => {
                    tracing::warn!(
                        peer = %self.cid,
                        lane = lane.index(),
                        received,
                        advertised,
                        "rejected data-channel message beyond the advertised credit: the peer \
                         broke flow control"
                    );
                    self.report_invalid_inbound_frame();
                    None
                }
            }
        }

        /// Receive one raw frame from a transport adapter without waiting: decode and admit it,
        /// and queue an admitted frame on its lane for in-order dispatch.
        pub fn receive_inbound_frame(self: &$shared<Self>, raw: Bytes, lane: ChannelLane) {
            if let Some(frame) = self.prepare_inbound_frame(raw, lane) {
                self.dispatch_admitted_frame(frame, lane);
            }
        }

        /// Queue an admitted frame on its lane, and start the lane's drainer if none runs.
        pub fn dispatch_admitted_frame(
            self: &$shared<Self>,
            frame: AdmittedInboundFrame,
            lane: ChannelLane,
        ) {
            let index = credit_index(lane);
            if !self.enqueue_frame(frame, index) {
                return;
            }
            let callback = $shared::clone(self);
            if rings_runtime::spawn_detached(async move { callback.drain_lane(index).await })
                .is_err()
            {
                let queue = &mut lock_recover(&self.lanes)[index];
                queue.draining = false;
                queue.frames.clear();
                tracing::error!(
                    peer = %self.cid,
                    "inbound dispatch requires a runtime; the lane's queued frames were dropped"
                );
            }
        }

        /// Synchronously record terminal credit/data-channel wakeup effects for `state`, then
        /// enqueue `state` for strictly serialized, arrival-ordered callback dispatch.
        pub fn dispatch_peer_connection_state(self: &$shared<Self>, state: TransportConnectionState) {
            if state.is_terminal() {
                self.close_link_credit();
                self.data_channel_state_notifier.wake();
            }
            if !self.enqueue_peer_state(state) {
                return;
            }
            let callback = $shared::clone(self);
            if rings_runtime::spawn_detached(async move { callback.drain_peer_states().await })
                .is_err()
            {
                let mut queue = lock_recover(&self.peer_states);
                queue.draining = false;
                queue.states.clear();
                tracing::error!(
                    peer = %self.cid,
                    "peer-connection state dispatch requires a runtime; queued state changes were dropped"
                );
            }
        }
    };
}

impl InnerTransportCallback {
    /// Bind a callback to one connection identifier, with the production credit windows, its
    /// data lanes lent beyond their floors from the node's `pool` (one per transport, shared by
    /// its connections).
    pub fn new(
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
        pool: NodeFramePool,
    ) -> Self {
        let cid: Arc<str> = Arc::from(cid);
        Self {
            link_credit: Arc::new(LinkCredit::new(Arc::clone(&cid), pool)),
            cid,
            callback,
            data_channel_state_notifier,
            #[cfg(rings_transport_runtime)]
            lanes: Mutex::default(),
            #[cfg(rings_transport_runtime)]
            peer_states: Mutex::default(),
            #[cfg(any(test, rings_transport_runtime))]
            invalid_frame_report_state: AtomicUsize::new(0),
        }
    }

    /// Return the immutable connection identifier bound to this callback.
    pub fn cid(&self) -> &str {
        &self.cid
    }

    #[cfg(any(test, rings_transport_backend))]
    /// This connection's credit flow control, for the backend's send path and credit pump.
    pub(crate) fn link_credit(&self) -> &Arc<LinkCredit> {
        &self.link_credit
    }

    /// Wait for one credit to send a custom frame on `lane`; see `LinkCredit::reserve`. The
    /// wait owns the link's credit state and nothing else of the connection: boxed, it is a
    /// backend's [`ConnectionInterface::reserve_send_credit`](crate::core::transport::ConnectionInterface::reserve_send_credit).
    pub fn reserve_credit(
        &self,
        lane: ChannelLane,
    ) -> impl Future<Output = Result<LaneCreditReservation>> + 'static {
        let link = Arc::clone(&self.link_credit);
        async move { link.reserve(lane).await.map(LaneCreditReservation) }
    }

    /// A read-only view of `lane`'s send credit; see
    /// [`ConnectionInterface::send_credit_view`](crate::core::transport::ConnectionInterface::send_credit_view).
    pub fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
        LaneCreditView::new(&self.link_credit, lane)
    }

    /// The credit a custom frame on `lane` is sent under, bound to the send `permit` admits: the
    /// credit `permit` carries or, without one, a credit reserved now.
    pub(crate) async fn credit_for_custom_send(
        &self,
        lane: ChannelLane,
        permit: &mut SendPermit,
    ) -> Result<CreditReservation> {
        let credit = match permit.take_credit() {
            Some(credit) if !credit.0.is_of(&self.link_credit) => {
                return Err(Error::ForeignCredit(self.cid.to_string()));
            }
            Some(credit) => credit,
            None => self.reserve_credit(lane).await?,
        };
        Ok(credit.0.bind(permit.acceptance()))
    }

    /// Run `send_raw` under one credit of `lane` (taken from `permit` or reserved now), encoding
    /// `payload` into its wire frame (`custom_frame_wire`), verifying `wire.len() <= max_message_size`,
    /// claiming `permit` into an [`IrrevocableSendPermit`](crate::core::transport::IrrevocableSendPermit),
    /// and enforcing generation retirement via `retire_generation` if `send_raw` fails or is
    /// cancelled before calling [`IrrevocableSendPermit::accept`](crate::core::transport::IrrevocableSendPermit::accept).
    ///
    /// External [`ConnectionInterface`](crate::core::transport::ConnectionInterface) backends use
    /// this adapter to implement `send_message_with_permit` with full type-enforced credit-window,
    /// frame-size, and post-irrevocable retirement guarantees.
    pub async fn send_custom_wire<R, F, Fut>(
        &self,
        payload: Bytes,
        lane: ChannelLane,
        mut permit: SendPermit,
        max_message_size: usize,
        retire_generation: R,
        send_raw: F,
    ) -> Result<crate::delivery::DeliveryFuture>
    where
        R: FnOnce(),
        F: FnOnce(Bytes, ChannelLane, crate::core::transport::IrrevocableSendPermit) -> Fut,
        Fut: Future<
            Output = Result<(
                crate::core::transport::AcceptedSendPermit,
                crate::delivery::DeliveryFuture,
            )>,
        >,
    {
        let _credit = self.credit_for_custom_send(lane, &mut permit).await?;
        let wire = rings_codec::serialize(&TransportMessage::Custom(payload)).map(Bytes::from)?;
        if wire.len() > max_message_size {
            return Err(Error::FrameTooLarge {
                size: wire.len(),
                max: max_message_size,
            });
        }
        let acceptance = permit.acceptance();
        let Some(irrevocable) = permit.try_mark_irrevocable() else {
            return Err(Error::SendPermitRevoked);
        };
        struct RetireOnIncomplete<R: FnOnce()> {
            acceptance: crate::core::transport::SendAcceptance,
            retire: Option<R>,
        }
        impl<R: FnOnce()> Drop for RetireOnIncomplete<R> {
            fn drop(&mut self) {
                if self.acceptance.failed_after_irrevocable() {
                    if let Some(retire) = self.retire.take() {
                        retire();
                    }
                }
            }
        }
        let mut guard = RetireOnIncomplete {
            acceptance,
            retire: Some(retire_generation),
        };
        let (_accepted, delivery) = send_raw(wire, lane, irrevocable).await?;
        guard.retire = None;
        Ok(delivery)
    }

    /// Wait for the next outbound wire-encoded credit control frame on `index`, returning
    /// `Ok(None)` once the connection's credit state is closed.
    ///
    /// Returns a RAII [`CreditFrame`] guard that automatically requeues the underlying cumulative
    /// credit signal on drop unless [`CreditFrame::mark_sent`] is called after wire transmission
    /// succeeds, preventing lost credit signals from stalling a remote sender.
    pub async fn next_credit_frame(&self, index: CreditIndex) -> Result<Option<CreditFrame>> {
        let Some(signal) = self.link_credit.next_signal(index).await else {
            return Ok(None);
        };
        match rings_codec::serialize(&TransportMessage::Signal(signal)) {
            Ok(bytes) => Ok(Some(CreditFrame {
                wire: Bytes::from(bytes),
                index,
                signal: Some(signal),
                link: Arc::downgrade(&self.link_credit),
            })),
            Err(err) => {
                let _ = self.link_credit.requeue(index, signal);
                Err(Error::Codec(err))
            }
        }
    }

    /// Run one idle-lane pool shrink pass (`Law 3`). Returns `false` once the connection's credit
    /// state is closed.
    pub fn idle_check(&self) -> bool {
        self.link_credit.idle_check()
    }

    /// Return a weak [`IdleLaneChecker`] handle that runs periodic [`Self::idle_check`] passes
    /// without keeping the connection or callback alive.
    pub fn weak_idle_checker(&self) -> IdleLaneChecker {
        IdleLaneChecker {
            link: Arc::downgrade(&self.link_credit),
        }
    }

    /// Spawn the complete per-lane credit signal pumps (with automatic requeue and
    /// [`CREDIT_RESEND_PAUSE`] retry backoff) plus the periodic [`super::LANE_IDLE_PERIOD`]
    /// weak idle-lane shrinker for an external backend.
    ///
    /// The `send_control` closure transmits a serialized control frame on `lane`, returning
    /// `Err(Error::ConnectionReleased(_))` once the physical connection has permanently closed.
    /// Returns [`Error::CreditPumpUnavailable`] if the runtime cannot schedule the driver tasks.
    #[cfg(rings_transport_runtime)]
    #[allow(clippy::disallowed_methods)]
    pub fn spawn_credit_driver<S, Fut>(&self, send_control: S) -> Result<()>
    where
        S: Fn(Bytes, ChannelLane) -> Fut + Clone + rings_runtime::MaybeSendSync + 'static,
        Fut: Future<Output = Result<()>> + rings_runtime::MaybeSend + 'static,
    {
        for index in CreditIndex::ALL {
            let link = Arc::clone(&self.link_credit);
            let send = send_control.clone();
            rings_runtime::spawn_detached(async move {
                while let Some(signal) = link.next_signal(index).await {
                    let wire = match rings_codec::serialize(&TransportMessage::Signal(signal)) {
                        Ok(bytes) => Bytes::from(bytes),
                        Err(error) => {
                            tracing::error!(peer = %link.peer, ?error, "failed to encode credit signal");
                            let _ = link.requeue(index, signal);
                            return;
                        }
                    };
                    let mut frame = CreditFrame {
                        wire: wire.clone(),
                        index,
                        signal: Some(signal),
                        link: Arc::downgrade(&link),
                    };
                    match send(wire, index.lane()).await {
                        Ok(()) => {
                            frame.mark_sent();
                        }
                        Err(Error::ConnectionReleased(_)) => {
                            frame.mark_sent();
                            return;
                        }
                        Err(error) => {
                            tracing::debug!(
                                peer = %link.peer,
                                ?error,
                                "external transport failed to send credit frame; backing off and requeuing"
                            );
                            drop(frame);
                            if rings_runtime::sleep(CREDIT_RESEND_PAUSE).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|_| Error::CreditPumpUnavailable(self.cid.to_string()))?;
        }
        let checker = self.weak_idle_checker();
        rings_runtime::spawn_detached(async move {
            loop {
                if rings_runtime::sleep(super::link_credit::LANE_IDLE_PERIOD)
                    .await
                    .is_err()
                {
                    return;
                }
                if !checker.check_once() {
                    return;
                }
            }
        })
        .map_err(|_| Error::CreditPumpUnavailable(self.cid.to_string()))?;
        Ok(())
    }

    /// Notify the data channel is open.
    pub async fn on_channel_open(&self) {
        self.on_channel_open_with_cid(&self.cid).await;
    }

    pub(crate) async fn on_channel_open_with_cid(&self, cid: &str) {
        self.data_channel_state_notifier.wake();
        if let Err(e) = self.callback.on_channel_open(cid).await {
            tracing::error!("Callback on_channel_open failed: {e:?}");
        }
    }

    /// Notify the data channel is close.
    pub async fn on_channel_close(&self) {
        self.on_channel_close_with_cid(&self.cid).await;
    }

    pub(crate) async fn on_channel_close_with_cid(&self, cid: &str) {
        self.data_channel_state_notifier.wake();
        if let Err(e) = self.callback.on_channel_close(cid).await {
            tracing::error!("Callback on_channel_close failed: {e:?}");
        }
    }

    /// Synchronously decode the transport envelope of a frame that arrived on `lane`, and either
    /// apply it (a credit) or admit it against the lane's credit window (a custom frame).
    ///
    /// Public transport adapters must call this before retaining the frame in an async task,
    /// then pass an admitted value to [`Self::handle_admitted_frame`]. Adapters that do not use
    /// the runtime-backed `prepare_inbound_frame` helper must call
    /// [`Self::notify_invalid_inbound_frame`] for `Malformed`, `Oversized` and `CreditExceeded`.
    pub fn admit_inbound_frame(&self, raw: Bytes, lane: ChannelLane) -> InboundFrameAdmission {
        if inbound_frame_exceeds_protocol_ceiling(raw.len()) {
            return InboundFrameAdmission::Oversized {
                bytes: raw.len(),
                max_bytes: MAX_CHANNEL_MESSAGE_SIZE,
            };
        }
        let (borrowed, remaining) =
            match rings_codec::deserialize_prefix::<BorrowedTransportMessage>(&raw) {
                Ok(decoded) => decoded,
                Err(error) => return InboundFrameAdmission::Malformed(error),
            };
        if !remaining.is_empty() {
            return InboundFrameAdmission::Malformed(rings_codec::Error::TrailingBytes {
                decoded: raw.len() - remaining.len(),
                total: raw.len(),
            });
        }
        let payload = match borrowed {
            BorrowedTransportMessage::Signal(signal) => {
                self.link_credit.apply(lane, signal);
                return InboundFrameAdmission::Credit;
            }
            BorrowedTransportMessage::Custom(payload) => payload,
        };
        let permit = match self.link_credit.admit(lane) {
            Ok(permit) => permit,
            Err(violation) => {
                return InboundFrameAdmission::CreditExceeded {
                    received: violation.received,
                    advertised: violation.advertised,
                }
            }
        };
        let payload = raw.slice_ref(payload);
        InboundFrameAdmission::Admitted(AdmittedInboundFrame { payload, permit })
    }

    #[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
    define_shared_inbound_frame_paths!(Arc);

    #[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
    define_shared_inbound_frame_paths!(Rc);

    /// Queue an admitted frame on its lane and, if no drainer owns the lane, drain it inline
    /// before returning.
    ///
    /// This is the interleaving of [`Self::dispatch_admitted_frame`] in which the drainer runs at
    /// once and to completion: the deterministic dummy scheduler delivers one event at a time
    /// and takes its delivery to mean the event was processed. Lane order is the same law.
    #[cfg(feature = "dummy")]
    pub(crate) async fn dispatch_admitted_frame_inline(
        &self,
        frame: AdmittedInboundFrame,
        lane: ChannelLane,
    ) {
        let index = credit_index(lane);
        if self.enqueue_frame(frame, index) {
            self.drain_lane(index).await;
        }
    }

    /// Queue `frame` on credit index `index`; `true` when the caller claimed the lane's drainer,
    /// which no task held, and must now drain the lane.
    #[cfg(rings_transport_runtime)]
    fn enqueue_frame(&self, frame: AdmittedInboundFrame, index: CreditIndex) -> bool {
        let queue = &mut lock_recover(&self.lanes)[index];
        queue.frames.push_back(frame);
        !mem::replace(&mut queue.draining, true)
    }

    /// Hand the queued frames of credit index `index` to the protocol callback, in order, until
    /// the queue is empty; the drainer then gives the queue up.
    ///
    /// Pre: the caller claimed the lane's drainer ([`Self::enqueue_frame`] returned `true`).
    #[cfg(rings_transport_runtime)]
    async fn drain_lane(&self, index: CreditIndex) {
        let mut drainer = LaneDrainer {
            callback: self,
            index,
            finished: false,
        };
        while let Some(frame) = drainer.next() {
            self.handle_admitted_frame(frame).await;
        }
    }

    #[cfg(rings_transport_runtime)]
    fn enqueue_peer_state(&self, state: TransportConnectionState) -> bool {
        let mut queue = lock_recover(&self.peer_states);
        queue.states.push_back(state);
        !mem::replace(&mut queue.draining, true)
    }

    #[cfg(rings_transport_runtime)]
    async fn drain_peer_states(&self) {
        let mut drainer = PeerStateDrainer {
            callback: self,
            finished: false,
        };
        while let Some(state) = drainer.next() {
            self.on_connection_state_change(state).await;
        }
    }

    /// Dispatch one admitted frame and transfer its credit permit to the callback.
    pub async fn handle_admitted_frame(&self, frame: AdmittedInboundFrame) {
        // Law (admission ownership). Admission draws the permit from this callback's link
        // credit, which this callback alone creates and no other callback holds as its own, so
        // a permit of that link proves the frame was admitted by this callback, on this
        // connection.
        if !frame.permit.is_of(&self.link_credit) {
            tracing::error!(peer = %self.cid, "rejected inbound frame admitted by another callback");
            return;
        }
        let AdmittedInboundFrame { payload, permit } = frame;
        let lane = permit.lane();
        let message =
            AdmittedInboundMessage::new(&self.cid, payload, lane, InboundCreditLease::new(permit));
        if let Err(error) = self.callback.on_admitted_message(message).await {
            tracing::error!("Callback on_admitted_message failed: {error:?}");
        }
    }

    /// This method is invoked when the state of connection has changed.
    pub async fn on_connection_state_change(&self, s: TransportConnectionState) {
        self.on_connection_state_change_with_cid(&self.cid, s).await;
    }

    pub(crate) async fn on_connection_state_change_with_cid(
        &self,
        cid: &str,
        s: TransportConnectionState,
    ) {
        if s.is_terminal() {
            self.close_link_credit();
            self.data_channel_state_notifier.wake();
        }
        if let Err(e) = self.callback.on_connection_state_change(cid, s).await {
            tracing::error!("Callback on_connection_state_change failed: {e:?}");
        }
    }
}

/// Exclusive claim of a running drainer task over [`PeerStateQueue`].
#[cfg(rings_transport_runtime)]
struct PeerStateDrainer<'a> {
    callback: &'a InnerTransportCallback,
    finished: bool,
}

#[cfg(rings_transport_runtime)]
impl PeerStateDrainer<'_> {
    fn next(&mut self) -> Option<TransportConnectionState> {
        let mut queue = lock_recover(&self.callback.peer_states);
        let next = queue.states.pop_front();
        if next.is_none() {
            queue.draining = false;
            self.finished = true;
        }
        next
    }
}

#[cfg(rings_transport_runtime)]
impl Drop for PeerStateDrainer<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let abandoned = {
            let mut queue = lock_recover(&self.callback.peer_states);
            queue.draining = false;
            mem::take(&mut queue.states)
        };
        tracing::warn!(
            peer = %self.callback.cid,
            states = abandoned.len(),
            "a peer-connection state drainer ended mid-dispatch; queued state changes were dropped"
        );
    }
}

/// The claim of one running drainer on its lane's queue.
///
/// Law (claim release). The claim ends with the drainer however it ends. A drainer that empties
/// the queue gives the lane up under the queue's lock; one cancelled or unwound mid-dispatch
/// gives it up on drop and drops the frames still queued, whose permits return their credit to
/// the peer. No lane is thus left claimed by a drainer that no longer runs, with its frames
/// holding the lane's window shut.
#[cfg(rings_transport_runtime)]
struct LaneDrainer<'a> {
    /// The callback whose lane is drained.
    callback: &'a InnerTransportCallback,
    /// The credit index of the lane.
    index: CreditIndex,
    /// Whether the drainer emptied the queue and gave the lane up.
    finished: bool,
}

#[cfg(rings_transport_runtime)]
impl LaneDrainer<'_> {
    /// The next queued frame, or `None` once the queue is empty, which gives the lane up.
    fn next(&mut self) -> Option<AdmittedInboundFrame> {
        let queue = &mut lock_recover(&self.callback.lanes)[self.index];
        let next = queue.frames.pop_front();
        if next.is_none() {
            queue.draining = false;
            self.finished = true;
        }
        next
    }
}

#[cfg(rings_transport_runtime)]
impl Drop for LaneDrainer<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let abandoned = {
            let queue = &mut lock_recover(&self.callback.lanes)[self.index];
            queue.draining = false;
            mem::take(&mut queue.frames)
        };
        tracing::warn!(
            peer = %self.callback.cid,
            lane = self.index.lane().index(),
            frames = abandoned.len(),
            "a lane drainer ended mid-dispatch; the lane's queued frames were dropped"
        );
    }
}

impl InnerTransportCallback {
    /// End the generation's credit: senders waiting for it fail and its credit pump stops.
    ///
    /// Law (no orphaned wait). A sender waiting for credit holds the connection, and through
    /// it this callback, so the callback's drop cannot be what releases the sender: the
    /// generation's end is. Every terminal state (`Failed`, `Closed`) closes the credit, and a
    /// backend whose local close emits no state change closes it there.
    pub fn close_link_credit(&self) {
        self.link_credit.close();
    }
}

impl Drop for InnerTransportCallback {
    /// The connection is gone: senders waiting for its credit fail and its credit pump stops.
    fn drop(&mut self) {
        self.close_link_credit();
    }
}
