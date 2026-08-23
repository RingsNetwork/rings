//! This module contains the [InnerTransportCallback] struct.

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::atomic::AtomicUsize;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::atomic::Ordering;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Arc;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Mutex;

use bytes::Bytes;

use crate::core::callback::BoxedTransportCallback;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::notifier::Notifier;

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
const INBOUND_FRAME_CAPACITY: usize = 256;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
const INBOUND_FRAME_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(crate) const INBOUND_DATA_CHANNEL_CAPACITY: usize = 4;

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[derive(Default)]
struct InboundFrameState {
    frames: usize,
    bytes: usize,
}

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
/// Node-wide bound held before a backend copies or dispatches an inbound frame.
pub(crate) struct InboundFrameCapacity {
    state: Mutex<InboundFrameState>,
}

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
impl InboundFrameCapacity {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(InboundFrameState::default()),
        }
    }

    pub(crate) fn try_acquire(self: &Arc<Self>, bytes: usize) -> Option<InboundFramePermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next_frames = state.frames.checked_add(1)?;
        let next_bytes = state.bytes.checked_add(bytes)?;
        if next_frames > INBOUND_FRAME_CAPACITY || next_bytes > INBOUND_FRAME_BYTE_CAPACITY {
            return None;
        }
        state.frames = next_frames;
        state.bytes = next_bytes;
        Some(InboundFramePermit {
            capacity: self.clone(),
            bytes,
        })
    }
}

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
/// RAII ownership of one admitted raw frame until its core callback completes.
pub(crate) struct InboundFramePermit {
    capacity: Arc<InboundFrameCapacity>,
    bytes: usize,
}

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
impl Drop for InboundFramePermit {
    fn drop(&mut self) {
        let mut state = self
            .capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frames = state.frames.saturating_sub(1);
        state.bytes = state.bytes.saturating_sub(self.bytes);
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
    /// The id of the connection to which the current callback is assigned.
    pub cid: String,
    callback: BoxedTransportCallback,
    data_channel_state_notifier: Notifier,
}

impl InnerTransportCallback {
    /// Create a new [InnerTransportCallback].
    pub fn new(
        cid: &str,
        callback: BoxedTransportCallback,
        data_channel_state_notifier: Notifier,
    ) -> Self {
        Self {
            cid: cid.to_string(),
            callback,
            data_channel_state_notifier,
        }
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

    /// This method is invoked on a binary message arrival over the data channel of webrtc.
    pub async fn on_message(&self, msg: &Bytes) {
        match rings_codec::deserialize(msg) {
            Ok(m) => self.handle_message(&m).await,
            Err(e) => {
                tracing::error!("Deserialize DataChannelMessage failed: {e:?}");
            }
        };
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

    async fn handle_message(&self, msg: &TransportMessage) {
        match msg {
            TransportMessage::Custom(bytes) => {
                if let Err(e) = self.callback.on_message(&self.cid, bytes).await {
                    tracing::error!("Callback on_message failed: {e:?}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_frame_capacity_releases_count_and_bytes_with_permit() {
        let capacity = Arc::new(InboundFrameCapacity::new());
        let permit = capacity
            .try_acquire(INBOUND_FRAME_BYTE_CAPACITY)
            .expect("one maximum-size frame must fit");
        assert!(capacity.try_acquire(1).is_none());
        drop(permit);
        assert!(capacity.try_acquire(1).is_some());
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
