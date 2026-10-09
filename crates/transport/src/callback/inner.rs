//! The callback bound to one connection: admission, credit, and per-lane dispatch.
//!
//! An arriving raw frame takes one of three paths, decided synchronously so that reading a
//! channel never waits on the protocol:
//!
//! ```text
//!   raw ─decode─▶ Credit(c)  ─▶ LinkCredit::grant(c)                  (applied at once)
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

#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::collections::VecDeque;
#[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
use std::rc::Rc;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::sync::Mutex;

use bytes::Bytes;

use super::inbound_frame_exceeds_protocol_ceiling;
#[cfg(rings_transport_backend)]
use super::link_credit::CreditReservation;
#[cfg(rings_transport_backend)]
use super::link_credit::LaneCreditReservation;
use super::link_credit::LinkCredit;
use super::link_credit::NodeReceiveLoad;
use super::AdmittedInboundFrame;
use super::InboundFrameAdmission;
use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::BoxedTransportCallback;
use crate::core::callback::InboundCreditLease;
#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use crate::core::credit::credit_index;
#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use crate::core::credit::CreditIndex;
use crate::core::credit::CreditWindow;
#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use crate::core::credit::PerLane;
use crate::core::pool::ChannelLane;
use crate::core::transport::BorrowedTransportMessage;
#[cfg(rings_transport_backend)]
use crate::core::transport::SendPermit;
#[cfg(rings_transport_backend)]
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
#[cfg(rings_transport_backend)]
use crate::error::Result;
use crate::notifier::Notifier;
#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use crate::sync_utils::lock_recover;

/// The frames of one lane admitted and not yet handed to the protocol callback.
#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
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
    #[cfg(any(
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    lanes: Mutex<PerLane<LaneQueue>>,
    admission_identity: Arc<()>,
    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    pub(super) invalid_frame_report_state: AtomicUsize,
}

#[cfg(any(
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
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
    /// Bind a callback to one connection identifier, with the production credit window, its
    /// receive load counted in the node's `load` (one per transport, shared by its
    /// connections).
    pub fn new(
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
        load: NodeReceiveLoad,
    ) -> Self {
        Self::with_window(
            cid,
            callback,
            data_channel_state_notifier,
            CreditWindow::PRODUCTION,
            load,
        )
    }

    /// Bind a callback to one connection identifier under the credit window `window`.
    pub(crate) fn with_window(
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
        window: CreditWindow,
        load: NodeReceiveLoad,
    ) -> Self {
        let cid: Arc<str> = Arc::from(cid);
        Self {
            link_credit: Arc::new(LinkCredit::new(Arc::clone(&cid), window, load)),
            cid,
            callback,
            data_channel_state_notifier,
            #[cfg(any(
                all(not(target_family = "wasm"), feature = "tokio"),
                all(target_family = "wasm", feature = "web-sys-webrtc")
            ))]
            lanes: Mutex::default(),
            admission_identity: Arc::new(()),
            #[cfg(any(
                test,
                all(not(target_family = "wasm"), feature = "tokio"),
                all(target_family = "wasm", feature = "web-sys-webrtc")
            ))]
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
    /// wait owns the link's credit state and nothing else of the connection.
    pub(crate) fn reserve_credit(
        &self,
        lane: ChannelLane,
    ) -> impl std::future::Future<Output = Result<LaneCreditReservation>> + 'static {
        let link = Arc::clone(&self.link_credit);
        async move { link.reserve(lane).await.map(LaneCreditReservation) }
    }

    #[cfg(rings_transport_backend)]
    /// The credit a frame on `lane` is sent under, bound to the send `permit` admits: for a
    /// custom frame, the credit `permit` carries or, without one, a credit reserved now; for a
    /// credit frame, none, since credit frames are never charged.
    pub(crate) async fn credit_for_send(
        &self,
        message: &TransportMessage,
        lane: ChannelLane,
        permit: &mut SendPermit,
    ) -> Result<Option<CreditReservation>> {
        let credit = match (message, permit.take_credit()) {
            (TransportMessage::Credit(_), _) => return Ok(None),
            (TransportMessage::Custom(_), Some(credit)) if !credit.0.is_of(&self.link_credit) => {
                return Err(crate::error::Error::ForeignCredit(self.cid.to_string()));
            }
            (TransportMessage::Custom(_), Some(credit)) => credit,
            (TransportMessage::Custom(_), None) => self.reserve_credit(lane).await?,
        };
        Ok(Some(credit.0.bind(permit.acceptance())))
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
                max_bytes: crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE,
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
            BorrowedTransportMessage::Credit(credit) => {
                self.link_credit.grant(lane, credit);
                return InboundFrameAdmission::Credit;
            }
            BorrowedTransportMessage::Custom(payload) => payload,
        };
        let permit = match self.link_credit.admit(lane, raw.len()) {
            Ok(permit) => permit,
            Err(violation) => {
                return InboundFrameAdmission::CreditExceeded {
                    received: violation.received,
                    advertised: violation.advertised,
                }
            }
        };
        let payload = raw.slice_ref(payload);
        InboundFrameAdmission::Admitted(AdmittedInboundFrame {
            payload,
            owner: Arc::clone(&self.admission_identity),
            permit,
        })
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
    #[cfg(any(
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    fn enqueue_frame(&self, frame: AdmittedInboundFrame, index: CreditIndex) -> bool {
        let queue = &mut lock_recover(&self.lanes)[index];
        queue.frames.push_back(frame);
        !std::mem::replace(&mut queue.draining, true)
    }

    /// Hand the queued frames of credit index `index` to the protocol callback, in order, until
    /// the queue is empty; the drainer then gives the queue up.
    #[cfg(any(
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    async fn drain_lane(&self, index: CreditIndex) {
        loop {
            let next = {
                let queue = &mut lock_recover(&self.lanes)[index];
                let next = queue.frames.pop_front();
                if next.is_none() {
                    queue.draining = false;
                }
                next
            };
            let Some(frame) = next else {
                return;
            };
            self.handle_admitted_frame(frame).await;
        }
    }

    /// Dispatch one admitted frame and transfer its credit permit to the callback.
    pub async fn handle_admitted_frame(&self, frame: AdmittedInboundFrame) {
        // Admission creates the owner identity and the credit permit together from this
        // immutable callback. Matching the owner therefore also proves the connection.
        if !Arc::ptr_eq(&self.admission_identity, &frame.owner) {
            tracing::error!(peer = %self.cid, "rejected inbound frame admitted by another callback");
            return;
        }
        let AdmittedInboundFrame {
            payload,
            owner: _,
            permit,
        } = frame;
        let message =
            AdmittedInboundMessage::new(&self.cid, payload, InboundCreditLease::new(permit));
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
