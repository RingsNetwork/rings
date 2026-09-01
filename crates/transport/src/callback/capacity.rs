use std::collections::BTreeMap;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::atomic::AtomicUsize;
#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;

pub(super) const INBOUND_FRAME_CAPACITY: usize = 256;
const INBOUND_FRAME_BYTE_CAPACITY: usize = 16 * 1024 * 1024;
pub(super) const INBOUND_PEER_FRAME_CAPACITY: usize = 64;
pub(super) const INBOUND_PEER_BYTE_CAPACITY: usize = 4 * 1024 * 1024;

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(super) const INBOUND_DATA_CHANNEL_CAPACITY: usize = 4;

#[derive(Default)]
struct InboundFrameState {
    frames: usize,
    bytes: usize,
    peers: BTreeMap<Arc<str>, PeerInboundFrameState>,
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

    pub(super) fn try_acquire_raw(
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
            let peer_state = state.peers.entry(Arc::clone(&peer)).or_default();
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

/// RAII ownership of one admitted raw frame until a downstream bounded queue takes ownership.
pub(crate) struct InboundFramePermit {
    capacity: Arc<InboundFrameCapacity>,
    pub(super) peer: Arc<str>,
    bytes: usize,
}

/// One decoded transport frame retaining raw capacity until downstream admission.
pub struct AdmittedInboundFrame {
    pub(super) payload: Bytes,
    pub(super) owner: Arc<()>,
    pub(super) permit: InboundFramePermit,
}

impl AdmittedInboundFrame {
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn payload(&self) -> &Bytes {
        &self.payload
    }
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
