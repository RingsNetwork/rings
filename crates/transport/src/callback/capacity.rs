//! Receive-side admission: a frame is admitted against its lane's credit window, and the
//! admission is held until the protocol callback takes the frame over.
//!
//! There is no node-wide frame budget to refuse an honest frame against: each connection holds
//! at most [`INBOUND_PEER_FRAME_CAPACITY`] frames, by the credit laws of [`crate::core::credit`],
//! and across connections the bound is the soft bound of
//! [`NodeReceiveLoad`](crate::callback::NodeReceiveLoad).

#[cfg(any(test, rings_transport_webrtc))]
use std::sync::atomic::AtomicU8;
#[cfg(any(test, rings_transport_webrtc))]
use std::sync::atomic::Ordering;

use bytes::Bytes;

use super::link_credit::CreditPermit;
use crate::core::credit::LANE_CREDIT_WINDOW;
#[cfg(any(test, rings_transport_webrtc))]
use crate::core::pool::ChannelLane;
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;

/// Raw frames one peer may have admitted at this end and not yet taken over by the protocol
/// callback: one credit window per lane, 64 frames of at most `MAX_DATA_CHANNEL_MESSAGE_SIZE`
/// bytes each, 4 MiB in all.
pub const INBOUND_PEER_FRAME_CAPACITY: usize =
    DATA_CHANNEL_POOL_SIZE as usize * LANE_CREDIT_WINDOW as usize;

/// The label prefix of a data channel; the lane index follows it.
#[cfg(any(test, rings_transport_webrtc))]
pub(crate) const DATA_CHANNEL_LABEL_PREFIX: &str = "rings_data_channel_";

/// The label of the data channel `lane` is pinned to.
#[cfg(any(test, rings_transport_webrtc))]
pub(crate) fn data_channel_label(lane: ChannelLane) -> String {
    format!("{DATA_CHANNEL_LABEL_PREFIX}{}", lane.index())
}

pub(crate) const fn inbound_frame_exceeds_protocol_ceiling(bytes: usize) -> bool {
    bytes > crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE
}

/// One decoded transport frame holding its lane's credit until downstream admission.
pub struct AdmittedInboundFrame {
    /// The frame's payload.
    pub(super) payload: Bytes,
    /// The lane credit the frame holds until it is handed off; dropping it returns the credit.
    pub(super) permit: CreditPermit,
}

impl AdmittedInboundFrame {
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn payload(&self) -> &Bytes {
        &self.payload
    }
}

/// Result of decoding and credit-admitting one raw backend frame.
pub enum InboundFrameAdmission {
    /// A custom frame decoded and took its place in its lane's credit window.
    Admitted(AdmittedInboundFrame),
    /// A credit frame decoded and was applied to this end's sends; nothing is dispatched.
    Credit,
    /// The transport envelope could not be decoded exactly.
    Malformed(rings_codec::Error),
    /// The frame exceeds the data-channel protocol ceiling.
    Oversized {
        /// Received wire bytes.
        bytes: usize,
        /// Maximum permitted wire bytes.
        max_bytes: usize,
    },
    /// The frame arrived beyond the credit this end advertised for its lane: the peer broke
    /// the flow-control protocol.
    CreditExceeded {
        /// Frames of the lane already received.
        received: u64,
        /// The credit advertised for the lane.
        advertised: u64,
    },
}

/// Admit a remote-created data channel by its label: the lane its label names, at most once.
///
/// Post: `Some(lane)` for the first channel labelled with `lane`, `lane < DATA_CHANNEL_POOL_SIZE`;
/// `None` for an unknown label or a lane that already has its channel, so a peer cannot open
/// more channels than the pool, nor two for one lane.
#[cfg(any(test, rings_transport_webrtc))]
pub(crate) fn admit_inbound_data_channel(admitted: &AtomicU8, label: &str) -> Option<ChannelLane> {
    let index = label
        .strip_prefix(DATA_CHANNEL_LABEL_PREFIX)?
        .parse::<u8>()
        .ok()
        .filter(|index| *index < DATA_CHANNEL_POOL_SIZE)?;
    let bit = 1u8 << index;
    let previous = admitted.fetch_or(bit, Ordering::AcqRel);
    (previous & bit == 0).then(|| ChannelLane::new(index))
}
