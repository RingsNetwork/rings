//! Media-channel types shared across backends.
//!
//! Where [`crate::core::transport`] carries reliable, message-oriented data-channel traffic, this
//! module models the *media* side as **media tracks**, the one shape both native (webrtc-rs) and
//! browser (web-sys `MediaStreamTrack`) expose identically. The upper layers (swarm / node) only
//! ever touch the [`MediaTrack`] trait, so media behaves the same on both platforms.
//!
//! What is uniform: negotiating, attaching, receiving, enabling and stopping a track. What is
//! inherently platform-specific (and so lives behind each backend's concrete track type, not here):
//! *acquiring* a local source (browser `getUserMedia`/canvas vs a native sample writer) and
//! *consuming* a remote track's media (browser `<video>` vs reading RTP). This mirrors WebRTC
//! itself, where `addTrack`/`ontrack` are uniform but `getUserMedia` is not.

use serde::Deserialize;
use serde::Serialize;

/// Which kind of media a track carries. Selects the `m=audio` / `m=video` SDP section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    /// An audio track.
    Audio,
    /// A video track.
    Video,
}

/// A platform-agnostic handle to one media track on a connection. Each backend supplies a concrete
/// type implementing this; everything above the transport deals only with the trait, so the media
/// API is identical on native and browser.
pub trait MediaTrack {
    /// The track's id (stable per track).
    fn id(&self) -> String;
    /// Whether this is an audio or video track.
    fn kind(&self) -> MediaKind;
    /// Whether the track is currently enabled (delivering media).
    fn enabled(&self) -> bool;
    /// Enable or mute the track without removing it.
    fn set_enabled(&self, enabled: bool);
    /// Downcast hook: a backend recovers its own concrete track from a [`BoxedMediaTrack`] handed to
    /// [`add_media_track`](crate::core::transport::ConnectionInterface::add_media_track). Implement
    /// as `fn as_any(&self) -> &dyn std::any::Any { self }`.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Boxed [`MediaTrack`] passed across the connection/callback boundary. `Send + Sync` off the
/// browser; single-threaded on it (same split as [`BoxedTransportCallback`](crate::core::callback)).
#[cfg(not(feature = "web-sys-webrtc"))]
pub type BoxedMediaTrack = Box<dyn MediaTrack + Send + Sync>;

/// Boxed [`MediaTrack`] passed across the connection/callback boundary.
#[cfg(feature = "web-sys-webrtc")]
pub type BoxedMediaTrack = Box<dyn MediaTrack>;

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

/// Errors from the media track path. Kept separate from a connection's `Error` so the trait's media
/// methods can carry a meaningful default ("unsupported") without every backend implementing them.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// This connection/backend has no media support (data-only, or a backend without media).
    #[error("media is not available on this connection")]
    Unsupported,
    /// The underlying peer connection rejected the track.
    #[error("failed to add media track: {0}")]
    AddTrack(String),
}
