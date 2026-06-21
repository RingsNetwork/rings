//! Media-channel types shared across backends.
//!
//! Where [`crate::core::transport`] carries reliable, message-oriented data-channel traffic, this
//! module describes the *media* side: opt-in RTP tracks negotiated on a connection. The transport
//! moves opaque RTP packets — it does not encode, decode, or capture; framing/jitter handling is a
//! receiver concern layered above (see `rings_core::media`).

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

/// Which kind of media an RTP track carries. Selects the `m=audio` / `m=video` SDP section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    /// An audio track.
    Audio,
    /// A video track.
    Video,
}

/// One RTP packet reduced to the fields a depacketizer needs (RFC 3550 header + payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    /// RTP sequence number (wraps at 16 bits).
    pub sequence: u16,
    /// RTP timestamp — the sampling instant; packets of one frame share it.
    pub timestamp: u32,
    /// Marker bit — set on the last packet of a frame (RFC 3550 §5.1).
    pub marker: bool,
    /// Opaque RTP payload bytes.
    pub payload: Bytes,
}

/// Configuration of one media track to negotiate on a connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaChannelConfig {
    /// Audio or video.
    pub kind: MediaKind,
    /// RTP payload type (the dynamic PT both peers agree on).
    pub payload_type: u8,
    /// RTP clock rate in Hz (e.g. 48000 for Opus, 90000 for video).
    pub clock_rate: u32,
}

/// Per-connection channel configuration. The reliable data channel is always present; a media track
/// is opt-in. `Default` is data-only, so existing call sites keep their behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// When set, a media track of this shape is negotiated alongside the data channel.
    pub media: Option<MediaChannelConfig>,
}

/// Errors from the media send path. Kept separate from a connection's `Error` so the trait's media
/// methods can carry a meaningful default ("unsupported") without every backend implementing them.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// This connection/backend has no media track (data-only, or a backend without media support).
    #[error("media is not available on this connection")]
    Unsupported,
    /// The underlying track rejected the packet.
    #[error("media send failed: {0}")]
    Send(String),
}
