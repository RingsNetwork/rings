#[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
use std::rc::Rc;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use bytes::Bytes;

use super::inbound_frame_exceeds_protocol_ceiling;
use super::AdmittedInboundFrame;
use super::InboundFrameAdmission;
use super::InboundFrameCapacity;
use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::BoxedTransportCallback;
use crate::core::callback::InboundFrameCapacityLease;
use crate::core::transport::BorrowedTransportMessage;
use crate::core::transport::TransportInterface;
use crate::core::transport::WebrtcConnectionState;
use crate::notifier::Notifier;

/// Wraps a transport callback with handling bound to one connection.
pub struct InnerTransportCallback {
    pub(super) cid: Arc<str>,
    pub(super) callback: BoxedTransportCallback,
    data_channel_state_notifier: Notifier,
    inbound_frames: Arc<InboundFrameCapacity>,
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
macro_rules! define_prepare_inbound_frame {
    ($shared:ident) => {
        /// Decode, capacity-admit, and report one raw frame from a transport adapter.
        ///
        /// Rejected malformed and oversized frames are reported to the callback;
        /// local capacity pressure is logged without degrading the remote peer.
        pub fn prepare_inbound_frame(
            self: &$shared<Self>,
            raw: Bytes,
        ) -> Option<AdmittedInboundFrame> {
            let received_bytes = raw.len();
            match self.admit_inbound_frame(raw) {
                InboundFrameAdmission::Admitted(frame) => Some(frame),
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
                // Local pressure, not evidence against the peer: logged at debug, since a
                // congested receiver refuses up to a whole window per link, and handed to the
                // callback, which counts the frame as released for its link's credit.
                InboundFrameAdmission::CapacityExceeded { payload } => {
                    tracing::debug!(
                        peer = %self.cid,
                        bytes = received_bytes,
                        "rejected data-channel message before dispatch"
                    );
                    self.callback
                        .on_inbound_frame_refused(&self.cid, payload.as_ref());
                    None
                }
            }
        }
    };
}

impl InnerTransportCallback {
    /// Bind a callback to one transport instance and connection identifier.
    pub fn for_transport<T: TransportInterface + ?Sized>(
        transport: &T,
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
    ) -> Self {
        Self::with_capacity(
            cid,
            callback,
            data_channel_state_notifier,
            Arc::clone(transport.inbound_frame_capacity()),
        )
    }

    fn with_capacity(
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
        inbound_frames: Arc<InboundFrameCapacity>,
    ) -> Self {
        Self {
            cid: Arc::from(cid),
            callback,
            data_channel_state_notifier,
            inbound_frames,
            admission_identity: Arc::new(()),
            #[cfg(any(
                test,
                all(not(target_family = "wasm"), feature = "tokio"),
                all(target_family = "wasm", feature = "web-sys-webrtc")
            ))]
            invalid_frame_report_state: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
        inbound_frames: Arc<InboundFrameCapacity>,
    ) -> Self {
        Self::with_capacity(cid, callback, data_channel_state_notifier, inbound_frames)
    }

    /// Return the immutable connection identifier bound to this callback.
    pub fn cid(&self) -> &str {
        &self.cid
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

    /// Synchronously decode the transport envelope and reserve raw-frame capacity.
    ///
    /// Public transport adapters must call this before retaining the frame in an
    /// async task, then pass an admitted value to [`Self::handle_admitted_frame`].
    /// Adapters that do not use the runtime-backed `prepare_inbound_frame` helper must call
    /// [`Self::notify_invalid_inbound_frame`] for `Malformed` and `Oversized`, and the
    /// callback's `on_inbound_frame_refused` for local `CapacityExceeded` rejections.
    pub fn admit_inbound_frame(&self, raw: Bytes) -> InboundFrameAdmission {
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
        let BorrowedTransportMessage::Custom(payload) = borrowed;
        let Some(permit) = self
            .inbound_frames
            .try_acquire_raw(Arc::clone(&self.cid), raw.len())
        else {
            return InboundFrameAdmission::CapacityExceeded {
                payload: raw.slice_ref(payload),
            };
        };
        let payload = raw.slice_ref(payload);
        InboundFrameAdmission::Admitted(AdmittedInboundFrame {
            payload,
            owner: Arc::clone(&self.admission_identity),
            permit,
        })
    }

    #[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
    define_prepare_inbound_frame!(Arc);

    #[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
    define_prepare_inbound_frame!(Rc);

    /// Dispatch one capacity-admitted frame and transfer its permit to the callback.
    pub async fn handle_admitted_frame(&self, frame: AdmittedInboundFrame) {
        // Admission creates the owner identity and peer permit together from this
        // immutable callback. Matching the owner therefore also proves the peer.
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
            AdmittedInboundMessage::new(&self.cid, payload, InboundFrameCapacityLease::new(permit));
        if let Err(error) = self.callback.on_admitted_message(message).await {
            tracing::error!("Callback on_admitted_message failed: {error:?}");
        }
    }

    /// Dummy builds: end a capacity-admitted frame undispatched, as this end's inbound bound
    /// refuses a frame: its capacity is released first, then the callback learns of the refusal
    /// with its payload, exactly as for a `CapacityExceeded` frame. A controlled schedule that
    /// withholds a frame for good ends it here, so a callback that paces its peer by the frames
    /// it released still counts it, as every production path that ends a frame does.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn refuse_admitted_frame(&self, frame: AdmittedInboundFrame) {
        let AdmittedInboundFrame {
            payload,
            owner,
            permit,
        } = frame;
        drop(permit);
        drop(owner);
        self.callback
            .on_inbound_frame_refused(&self.cid, payload.as_ref());
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
        if let Err(e) = self.callback.on_peer_connection_state_change(cid, s).await {
            tracing::error!("Callback on_peer_connection_state_change failed: {e:?}");
        }
    }
}
