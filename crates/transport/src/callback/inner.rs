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
#[cfg(rings_transport_backend)]
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

use bytes::Bytes;

use super::inbound_frame_exceeds_protocol_ceiling;
#[cfg(rings_transport_backend)]
use super::link_credit::CreditReservation;
#[cfg(rings_transport_backend)]
use super::link_credit::LaneCreditReservation;
#[cfg(rings_transport_backend)]
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
#[cfg(rings_transport_runtime)]
use crate::core::credit::CreditIndex;
#[cfg(rings_transport_runtime)]
use crate::core::credit::PerLane;
use crate::core::pool::ChannelLane;
use crate::core::transport::BorrowedTransportMessage;
#[cfg(rings_transport_backend)]
use crate::core::transport::SendPermit;
use crate::core::transport::WebrtcConnectionState;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
#[cfg(rings_transport_backend)]
use crate::error::Error;
#[cfg(rings_transport_backend)]
use crate::error::Result;
use crate::notifier::Notifier;
#[cfg(rings_transport_runtime)]
use crate::sync_utils::lock_recover;

/// The frames of one lane admitted and not yet handed to the protocol callback.
#[cfg(rings_transport_runtime)]
#[derive(Default)]
struct LaneQueue {
    /// Admitted frames in arrival order.
    frames: VecDeque<AdmittedInboundFrame>,
    /// Whether a drainer owns the queue.
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

    #[cfg(rings_transport_backend)]
    /// Wait for one credit to send a custom frame on `lane`; see [`LinkCredit::reserve`]. The
    /// wait owns the link's credit state and nothing else of the connection: boxed, it is a
    /// backend's `ConnectionInterface::reserve_send_credit`.
    pub(crate) fn reserve_credit(
        &self,
        lane: ChannelLane,
    ) -> impl Future<Output = Result<LaneCreditReservation>> + 'static {
        let link = Arc::clone(&self.link_credit);
        async move { link.reserve(lane).await.map(LaneCreditReservation) }
    }

    #[cfg(rings_transport_backend)]
    /// A read-only view of `lane`'s send credit; see
    /// [`ConnectionInterface::send_credit_view`](crate::core::transport::ConnectionInterface).
    pub(crate) fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
        LaneCreditView::new(&self.link_credit, lane)
    }

    #[cfg(rings_transport_backend)]
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

    /// Notify the data channel is open.
    pub async fn on_data_channel_open(&self) {
        self.on_data_channel_open_with_cid(&self.cid).await;
    }

    pub(crate) async fn on_data_channel_open_with_cid(&self, cid: &str) {
        self.data_channel_state_notifier.wake();
        if let Err(e) = self.callback.on_data_channel_open(cid).await {
            tracing::error!("Callback on_data_channel_open failed: {e:?}");
        }
    }

    /// Notify the data channel is close.
    pub async fn on_data_channel_close(&self) {
        self.on_data_channel_close_with_cid(&self.cid).await;
    }

    pub(crate) async fn on_data_channel_close_with_cid(&self, cid: &str) {
        self.data_channel_state_notifier.wake();
        if let Err(e) = self.callback.on_data_channel_close(cid).await {
            tracing::error!("Callback on_data_channel_close failed: {e:?}");
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
                max_bytes: MAX_DATA_CHANNEL_MESSAGE_SIZE,
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
    pub async fn on_peer_connection_state_change(&self, s: WebrtcConnectionState) {
        self.on_peer_connection_state_change_with_cid(&self.cid, s)
            .await;
    }

    pub(crate) async fn on_peer_connection_state_change_with_cid(
        &self,
        cid: &str,
        s: WebrtcConnectionState,
    ) {
        if s.is_terminal() {
            self.close_link_credit();
        }
        if let Err(e) = self.callback.on_peer_connection_state_change(cid, s).await {
            tracing::error!("Callback on_peer_connection_state_change failed: {e:?}");
        }
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
    pub(crate) fn close_link_credit(&self) {
        self.link_credit.close();
    }
}

impl Drop for InnerTransportCallback {
    /// The connection is gone: senders waiting for its credit fail and its credit pump stops.
    fn drop(&mut self) {
        self.close_link_credit();
    }
}
