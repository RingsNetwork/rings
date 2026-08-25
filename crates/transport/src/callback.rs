//! This module contains the [InnerTransportCallback] struct.

use std::collections::BTreeMap;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::future::poll_fn;
#[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
use std::rc::Rc;
#[cfg(any(
    test,
    feature = "native-webrtc",
    feature = "dummy",
    feature = "web-sys-webrtc",
    feature = "tokio"
))]
use std::sync::atomic::AtomicUsize;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::task::Poll;

use bytes::Bytes;

use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::BoxedTransportCallback;
use crate::core::transport::BorrowedTransportMessage;
use crate::core::transport::TransportInterface;
#[cfg(test)]
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::notifier::Notifier;

const INBOUND_FRAME_CAPACITY: usize = 256;
const INBOUND_FRAME_BYTE_CAPACITY: usize = 16 * 1024 * 1024;
const INBOUND_PEER_FRAME_CAPACITY: usize = 64;
const INBOUND_PEER_BYTE_CAPACITY: usize = 4 * 1024 * 1024;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const INVALID_FRAME_REPORT_BACKLOG_CAPACITY: usize = 256;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const INVALID_FRAME_REPORT_QUANTUM: usize = 32;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const INVALID_FRAME_WORKER_ACTIVE: usize = 1 << (usize::BITS - 1);
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const INVALID_FRAME_REPORT_COUNT_MASK: usize = INVALID_FRAME_WORKER_ACTIVE - 1;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(crate) const INBOUND_DATA_CHANNEL_CAPACITY: usize = 4;

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const _: () = {
    assert!(INVALID_FRAME_REPORT_BACKLOG_CAPACITY <= INVALID_FRAME_REPORT_COUNT_MASK);
};

#[derive(Default)]
struct InboundFrameState {
    frames: usize,
    bytes: usize,
    peers: BTreeMap<String, PeerInboundFrameState>,
}

#[derive(Default)]
struct PeerInboundFrameState {
    frames: usize,
    bytes: usize,
}

/// Node-wide bound held before a backend retains a frame for async dispatch.
pub struct InboundFrameCapacity {
    state: Mutex<InboundFrameState>,
}

impl InboundFrameCapacity {
    /// Construct one capacity accountant to share across every connection in a transport.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InboundFrameState::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn try_acquire(
        self: &Arc<Self>,
        peer: &str,
        bytes: usize,
    ) -> Option<InboundFramePermit> {
        self.try_acquire_raw(Arc::from(peer), bytes)
    }

    fn try_acquire_raw(
        self: &Arc<Self>,
        peer: Arc<str>,
        bytes: usize,
    ) -> Option<InboundFramePermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next_frames = state.frames.checked_add(1)?;
        let next_bytes = state.bytes.checked_add(bytes)?;
        if next_frames > INBOUND_FRAME_CAPACITY || next_bytes > INBOUND_FRAME_BYTE_CAPACITY {
            return None;
        }
        let peer_state = state.peers.get(peer.as_ref());
        let next_peer_frames = peer_state.map_or(0, |state| state.frames).checked_add(1)?;
        let next_peer_bytes = peer_state
            .map_or(0, |state| state.bytes)
            .checked_add(bytes)?;
        if next_peer_frames > INBOUND_PEER_FRAME_CAPACITY
            || next_peer_bytes > INBOUND_PEER_BYTE_CAPACITY
        {
            return None;
        }
        {
            let peer_state = state.peers.entry(peer.to_string()).or_default();
            peer_state.frames = next_peer_frames;
            peer_state.bytes = next_peer_bytes;
        }
        state.frames = next_frames;
        state.bytes = next_bytes;
        Some(InboundFramePermit {
            capacity: self.clone(),
            peer,
            bytes,
        })
    }
}

impl Default for InboundFrameCapacity {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, feature = "dummy"))]
pub(crate) const fn inbound_peer_frame_capacity_for_test() -> usize {
    INBOUND_PEER_FRAME_CAPACITY
}

pub(crate) const fn inbound_frame_exceeds_protocol_ceiling(bytes: usize) -> bool {
    bytes > crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE
}

/// RAII ownership of one admitted raw frame until its core callback completes.
pub(crate) struct InboundFramePermit {
    capacity: Arc<InboundFrameCapacity>,
    peer: Arc<str>,
    bytes: usize,
}

/// One decoded transport frame retaining its raw-frame capacity until callback completion.
pub struct AdmittedInboundFrame {
    payload: Bytes,
    owner: Arc<()>,
    _permit: InboundFramePermit,
}

/// Result of decoding and capacity-admitting one raw backend frame.
pub enum InboundFrameAdmission {
    /// The frame decoded and reserved raw capacity successfully.
    Admitted(AdmittedInboundFrame),
    /// The transport envelope could not be decoded exactly.
    Malformed(rings_codec::Error),
    /// The frame exceeds the data-channel protocol ceiling.
    Oversized {
        /// Received wire bytes.
        bytes: usize,
        /// Maximum permitted wire bytes.
        max_bytes: usize,
    },
    /// Node-wide or per-peer raw-frame capacity was unavailable.
    CapacityExceeded,
}

impl Drop for InboundFramePermit {
    fn drop(&mut self) {
        let mut state = self
            .capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frames = state.frames.saturating_sub(1);
        state.bytes = state.bytes.saturating_sub(self.bytes);
        if let Some(peer_state) = state.peers.get_mut(self.peer.as_ref()) {
            peer_state.frames = peer_state.frames.saturating_sub(1);
            peer_state.bytes = peer_state.bytes.saturating_sub(self.bytes);
            if peer_state.frames == 0 {
                state.peers.remove(self.peer.as_ref());
            }
        }
    }
}

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
/// Admit at most the protocol's fixed number of remote-created channels.
pub(crate) fn admit_inbound_data_channel(admitted: &AtomicUsize) -> bool {
    admitted
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < INBOUND_DATA_CHANNEL_CAPACITY).then_some(current + 1)
        })
        .is_ok()
}

/// [InnerTransportCallback] wraps the [BoxedTransportCallback] with inner handling for a specific connection.
pub struct InnerTransportCallback {
    cid: Arc<str>,
    callback: BoxedTransportCallback,
    data_channel_state_notifier: Notifier,
    inbound_frames: Arc<InboundFrameCapacity>,
    admission_identity: Arc<()>,
    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    invalid_frame_report_state: AtomicUsize,
}

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
struct InvalidFrameWorkerGuard<'a> {
    state: &'a AtomicUsize,
    armed: bool,
}

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
impl<'a> InvalidFrameWorkerGuard<'a> {
    const fn new(state: &'a AtomicUsize) -> Self {
        Self { state, armed: true }
    }
}

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
impl Drop for InvalidFrameWorkerGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Cancellation is terminal for this best-effort measurement batch.
            // One atomic swap prevents a producer from observing a stale active
            // worker: a producer before the swap is discarded with the batch;
            // one after it observes idle state and starts a replacement.
            self.state.swap(0, Ordering::AcqRel);
        }
    }
}

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
async fn yield_invalid_frame_report_worker() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
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
                InboundFrameAdmission::CapacityExceeded => {
                    tracing::warn!(
                        peer = %self.cid,
                        bytes = received_bytes,
                        "rejected data-channel message before dispatch"
                    );
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
    /// [`Self::notify_invalid_inbound_frame`] for `Malformed` and `Oversized`,
    /// but not for local `CapacityExceeded` rejections.
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
            return InboundFrameAdmission::CapacityExceeded;
        };
        let payload = raw.slice_ref(payload);
        InboundFrameAdmission::Admitted(AdmittedInboundFrame {
            payload,
            owner: Arc::clone(&self.admission_identity),
            _permit: permit,
        })
    }

    #[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
    define_prepare_inbound_frame!(Arc);

    #[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
    define_prepare_inbound_frame!(Rc);

    /// Dispatch one capacity-admitted frame and release its permit on completion.
    pub async fn handle_admitted_frame(&self, frame: AdmittedInboundFrame) {
        if !Arc::ptr_eq(&self.admission_identity, &frame.owner)
            || frame._permit.peer.as_ref() != self.cid.as_ref()
        {
            tracing::error!(peer = %self.cid, "rejected inbound frame admitted by another callback");
            return;
        }
        let message = AdmittedInboundMessage::new(&self.cid, &frame.payload);
        if let Err(error) = self.callback.on_admitted_message(message).await {
            tracing::error!("Callback on_admitted_message failed: {error:?}");
        }
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    fn queue_invalid_inbound_frame(&self) -> bool {
        self.invalid_frame_report_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let pending = (state & INVALID_FRAME_REPORT_COUNT_MASK)
                    .saturating_add(1)
                    .min(INVALID_FRAME_REPORT_BACKLOG_CAPACITY);
                Some(INVALID_FRAME_WORKER_ACTIVE | pending)
            })
            .map(|previous| previous & INVALID_FRAME_WORKER_ACTIVE == 0)
            .unwrap_or(false)
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    fn take_invalid_inbound_frame(&self) -> bool {
        self.invalid_frame_report_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                ((state & INVALID_FRAME_REPORT_COUNT_MASK) > 0).then_some(state - 1)
            })
            .is_ok()
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    fn release_invalid_frame_worker_if_idle(&self) -> bool {
        self.invalid_frame_report_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                ((state & INVALID_FRAME_REPORT_COUNT_MASK) == 0).then_some(0)
            })
            .is_ok()
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    async fn drain_invalid_inbound_frames(&self) {
        let mut active_guard = InvalidFrameWorkerGuard::new(&self.invalid_frame_report_state);
        loop {
            let mut processed = 0;
            while processed < INVALID_FRAME_REPORT_QUANTUM && self.take_invalid_inbound_frame() {
                self.notify_invalid_inbound_frame().await;
                processed += 1;
            }

            if self.release_invalid_frame_worker_if_idle() {
                active_guard.armed = false;
                return;
            }
            yield_invalid_frame_report_worker().await;
        }
    }

    /// Notify the callback about one malformed or oversized inbound frame.
    ///
    /// Custom adapters without a built-in runtime feature can spawn or await
    /// this method after [`Self::admit_inbound_frame`] rejects remote-invalid
    /// input. Local capacity pressure must not call it.
    pub async fn notify_invalid_inbound_frame(&self) {
        if let Err(error) = self.callback.on_invalid_inbound_frame(&self.cid).await {
            tracing::error!("Callback on_invalid_inbound_frame failed: {error:?}");
        }
    }

    #[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
    /// Report one malformed or oversized frame without blocking adapter ingress.
    ///
    /// Adapters should normally use [`Self::prepare_inbound_frame`], which calls
    /// this method only for remote-invalid input. This lower-level entry point is
    /// available for platform decoding failures that occur before a `Bytes`
    /// frame can be constructed.
    pub fn report_invalid_inbound_frame(self: &Arc<Self>) {
        if !self.queue_invalid_inbound_frame() {
            return;
        }
        let callback = Arc::clone(self);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            callback
                .invalid_frame_report_state
                .swap(0, Ordering::AcqRel);
            tracing::error!(peer = %callback.cid, "invalid-frame reporter requires a Tokio runtime");
            return;
        };
        runtime.spawn(async move { callback.drain_invalid_inbound_frames().await });
    }

    #[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
    /// Report one malformed or oversized frame without blocking adapter ingress.
    ///
    /// Adapters should normally use [`Self::prepare_inbound_frame`], which calls
    /// this method only for remote-invalid input. This lower-level entry point is
    /// available for platform decoding failures that occur before a `Bytes`
    /// frame can be constructed.
    pub fn report_invalid_inbound_frame(self: &Rc<Self>) {
        if self.queue_invalid_inbound_frame() {
            let callback = Rc::clone(self);
            wasm_bindgen_futures::spawn_local(async move {
                callback.drain_invalid_inbound_frames().await;
            });
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    fn pending_invalid_frame_count_for_test(&self) -> usize {
        self.invalid_frame_report_state.load(Ordering::Acquire) & INVALID_FRAME_REPORT_COUNT_MASK
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

#[cfg(test)]
mod tests {
    #[cfg(not(target_family = "wasm"))]
    use async_trait::async_trait;

    use super::*;
    #[cfg(not(target_family = "wasm"))]
    use crate::core::callback::TransportCallback;
    use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

    #[cfg(not(target_family = "wasm"))]
    type AdmittedPayloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    #[cfg(not(target_family = "wasm"))]
    struct RecordingCallback {
        admitted: AdmittedPayloads,
    }

    #[cfg(not(target_family = "wasm"))]
    struct InvalidRecordingCallback {
        invalid: Arc<AtomicUsize>,
    }

    #[cfg(not(target_family = "wasm"))]
    struct PendingInvalidCallback;

    #[cfg(not(target_family = "wasm"))]
    #[async_trait]
    impl TransportCallback for RecordingCallback {
        async fn on_admitted_message(
            &self,
            message: AdmittedInboundMessage<'_>,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            let (cid, payload) = message.into_parts();
            self.admitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((cid.to_owned(), payload.to_vec()));
            Ok(())
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[async_trait]
    impl TransportCallback for InvalidRecordingCallback {
        async fn on_invalid_inbound_frame(
            &self,
            _cid: &str,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            self.invalid.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[async_trait]
    impl TransportCallback for PendingInvalidCallback {
        async fn on_invalid_inbound_frame(
            &self,
            _cid: &str,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            std::future::pending().await
        }
    }

    #[test]
    fn raw_frame_capacity_releases_count_and_bytes_with_permit() {
        let capacity = Arc::new(InboundFrameCapacity::new());
        let permit = capacity
            .try_acquire("peer-a", INBOUND_PEER_BYTE_CAPACITY)
            .expect("one peer's byte allowance must fit");
        assert!(capacity.try_acquire("peer-a", 1).is_none());
        drop(permit);
        assert!(capacity.try_acquire("peer-a", 1).is_some());
    }

    #[test]
    fn one_peer_cannot_exhaust_another_peers_allowance() {
        let capacity = Arc::new(InboundFrameCapacity::new());
        let permits = (0..INBOUND_PEER_FRAME_CAPACITY)
            .map(|_| capacity.try_acquire("noisy", 1))
            .collect::<Option<Vec<_>>>()
            .expect("one peer may use its complete frame allowance");

        assert!(capacity.try_acquire("noisy", 1).is_none());
        assert!(capacity.try_acquire("other", 1).is_some());
        drop(permits);
    }

    #[test]
    fn raw_frame_capacity_is_shared_node_wide() {
        let capacity = Arc::new(InboundFrameCapacity::new());
        let permits = (0..INBOUND_FRAME_CAPACITY)
            .map(|index| {
                capacity.try_acquire(
                    &format!("peer-{}", index / INBOUND_PEER_FRAME_CAPACITY),
                    MAX_DATA_CHANNEL_MESSAGE_SIZE,
                )
            })
            .collect::<Option<Vec<_>>>()
            .expect("all node-wide frame slots must be available");

        assert!(capacity
            .try_acquire("extra", MAX_DATA_CHANNEL_MESSAGE_SIZE)
            .is_none());
        drop(permits);
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn admission_dispatches_decoded_payload_once() {
        let admitted = Arc::new(Mutex::new(Vec::new()));
        let capacity = Arc::new(InboundFrameCapacity::new());
        let callback = InnerTransportCallback::new_for_test(
            "peer",
            Box::new(RecordingCallback {
                admitted: Arc::clone(&admitted),
            }),
            Notifier::default(),
            Arc::clone(&capacity),
        );
        let data = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("data frame must serialize");

        let frame = match callback.admit_inbound_frame(Bytes::from(data)) {
            InboundFrameAdmission::Admitted(frame) => frame,
            _ => panic!("data frame must be admitted"),
        };
        assert!(admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
        callback.handle_admitted_frame(frame).await;
        assert_eq!(
            admitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[("peer".to_owned(), b"data".to_vec())]
        );
        assert!(matches!(
            callback.admit_inbound_frame(Bytes::from_static(b"malformed")),
            InboundFrameAdmission::Malformed(_)
        ));
    }

    #[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
    #[tokio::test]
    async fn prepare_inbound_frame_reports_remote_invalid_but_not_local_capacity() {
        let invalid = Arc::new(AtomicUsize::new(0));
        let callback = Arc::new(InnerTransportCallback::new_for_test(
            "peer",
            Box::new(InvalidRecordingCallback {
                invalid: Arc::clone(&invalid),
            }),
            Notifier::default(),
            Arc::new(InboundFrameCapacity::new()),
        ));
        let valid = Bytes::from(
            rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
                .expect("valid frame must serialize"),
        );

        assert!(callback.prepare_inbound_frame(valid.clone()).is_some());
        assert!(callback
            .prepare_inbound_frame(Bytes::from_static(b"malformed"))
            .is_none());
        assert!(callback
            .prepare_inbound_frame(Bytes::from(vec![
                0;
                crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE
                    + 1
            ]))
            .is_none());

        let held = (0..INBOUND_PEER_FRAME_CAPACITY)
            .map(|_| {
                callback
                    .prepare_inbound_frame(valid.clone())
                    .expect("peer frame reservation must remain available")
            })
            .collect::<Vec<_>>();
        assert!(callback.prepare_inbound_frame(valid).is_none());

        for _ in 0..16 {
            if invalid.load(Ordering::Acquire) == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(invalid.load(Ordering::Acquire), 2);
        drop(held);
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn admitted_frame_cannot_cross_callback_instances() {
        let admitted = Arc::new(Mutex::new(Vec::new()));
        let capacity = Arc::new(InboundFrameCapacity::new());
        let callback = |cid| {
            InnerTransportCallback::new_for_test(
                cid,
                Box::new(RecordingCallback {
                    admitted: Arc::clone(&admitted),
                }),
                Notifier::default(),
                Arc::clone(&capacity),
            )
        };
        let source = callback("source");
        let destination = callback("destination");
        let raw = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("data frame must serialize");
        let frame = match source.admit_inbound_frame(Bytes::from(raw)) {
            InboundFrameAdmission::Admitted(frame) => frame,
            _ => panic!("source callback must admit the frame"),
        };

        destination.handle_admitted_frame(frame).await;

        assert!(admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn invalid_frame_reports_use_one_coalesced_worker() {
        let invalid = Arc::new(AtomicUsize::new(0));
        let callback = InnerTransportCallback::new_for_test(
            "peer",
            Box::new(InvalidRecordingCallback {
                invalid: Arc::clone(&invalid),
            }),
            Notifier::default(),
            Arc::new(InboundFrameCapacity::new()),
        );

        for index in 0..64 {
            assert_eq!(callback.queue_invalid_inbound_frame(), index == 0);
        }
        callback.drain_invalid_inbound_frames().await;

        assert_eq!(invalid.load(Ordering::Acquire), 64);
        assert!(callback.queue_invalid_inbound_frame());
        callback.drain_invalid_inbound_frames().await;
        assert_eq!(invalid.load(Ordering::Acquire), 65);
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn cancelling_invalid_frame_worker_discards_backlog_and_allows_replacement() {
        let callback = InnerTransportCallback::new_for_test(
            "peer",
            Box::new(PendingInvalidCallback),
            Notifier::default(),
            Arc::new(InboundFrameCapacity::new()),
        );
        assert!(callback.queue_invalid_inbound_frame());
        let mut drain = Box::pin(callback.drain_invalid_inbound_frames());
        assert!(futures::poll!(&mut drain).is_pending());
        drop(drain);

        assert_eq!(callback.pending_invalid_frame_count_for_test(), 0);
        assert!(callback.queue_invalid_inbound_frame());
        assert_eq!(callback.pending_invalid_frame_count_for_test(), 1);
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn invalid_frame_backlog_is_bounded_and_yields_between_quanta() {
        let invalid = Arc::new(AtomicUsize::new(0));
        let callback = InnerTransportCallback::new_for_test(
            "peer",
            Box::new(InvalidRecordingCallback {
                invalid: Arc::clone(&invalid),
            }),
            Notifier::default(),
            Arc::new(InboundFrameCapacity::new()),
        );
        for _ in 0..INVALID_FRAME_REPORT_BACKLOG_CAPACITY.saturating_add(32) {
            callback.queue_invalid_inbound_frame();
        }
        assert_eq!(
            callback.pending_invalid_frame_count_for_test(),
            INVALID_FRAME_REPORT_BACKLOG_CAPACITY
        );

        let mut drain = Box::pin(callback.drain_invalid_inbound_frames());
        assert!(futures::poll!(&mut drain).is_pending());
        assert_eq!(
            invalid.load(Ordering::Acquire),
            INVALID_FRAME_REPORT_QUANTUM
        );
        drain.await;

        assert_eq!(
            invalid.load(Ordering::Acquire),
            INVALID_FRAME_REPORT_BACKLOG_CAPACITY
        );
        assert_eq!(callback.pending_invalid_frame_count_for_test(), 0);
    }

    #[test]
    fn borrowed_and_owned_transport_envelopes_share_the_complete_wire_schema() {
        let messages = [TransportMessage::Custom(Bytes::from_static(b"payload"))];

        for message in messages {
            let raw = rings_codec::serialize(&message).expect("transport frame must serialize");
            let (borrowed, remaining) =
                rings_codec::deserialize_prefix::<BorrowedTransportMessage>(&raw)
                    .expect("borrowed envelope must decode every owned variant");
            assert!(remaining.is_empty());
            match (message, borrowed) {
                (TransportMessage::Custom(owned), BorrowedTransportMessage::Custom(view)) => {
                    assert_eq!(owned.as_ref(), view);
                }
            }
        }
    }

    #[test]
    fn inbound_data_channel_count_is_monotonic_and_bounded() {
        let admitted = AtomicUsize::new(0);
        for _ in 0..INBOUND_DATA_CHANNEL_CAPACITY {
            assert!(admit_inbound_data_channel(&admitted));
        }
        assert!(!admit_inbound_data_channel(&admitted));
    }
}
