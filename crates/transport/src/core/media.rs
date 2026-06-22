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

/// The uniform metadata every media track exposes, regardless of platform or direction. This is the
/// *abstraction* the upper layers (swarm / node) rely on; it carries no platform-specific source or
/// sink, so there is nothing to downcast through here.
///
/// The two directions are typed separately, because each has one irreducibly platform-specific
/// operation that does not belong on this shared trait:
///
/// - **Outbound** — a local track is constructed per platform and attached with
///   [`add_media_track`](crate::core::transport::ConnectionInterface::add_media_track), whose
///   parameter is the backend's own concrete
///   [`LocalMediaTrack`](crate::core::transport::ConnectionInterface::LocalMediaTrack) type. There is
///   no boxing or downcast on this path.
/// - **Inbound** — a remote track is delivered as a [`RemoteMediaTrack`]; *consuming* its media
///   (native: read RTP; browser: attach the `MediaStreamTrack` to a sink) is platform-specific, so
///   that one boundary exposes a downcast — see [`RemoteMediaTrack::as_any`].
pub trait MediaTrack {
    /// The track's id (stable per track).
    fn id(&self) -> String;
    /// Whether this is an audio or video track.
    fn kind(&self) -> MediaKind;
    /// Whether the track is currently enabled (delivering media).
    fn enabled(&self) -> bool;
    /// Enable or mute the track without removing it.
    fn set_enabled(&self, enabled: bool);
}

/// A remote track delivered to [`on_media_track`](crate::core::callback::TransportCallback::on_media_track).
/// Metadata is uniform (via [`MediaTrack`]); *consuming* the media is irreducibly platform-specific
/// (native reads RTP off a `NativeRemoteTrack`; browser attaches a `MediaStreamTrack` to a sink), so
/// the concrete type is recovered here — and only here — via [`as_any`](Self::as_any). This isolates
/// the one unavoidable downcast to the inbound boundary instead of putting it on the shared trait.
pub trait RemoteMediaTrack: MediaTrack {
    /// Downcast hook: recover the backend's concrete remote track. Implement as
    /// `fn as_any(&self) -> &dyn std::any::Any { self }`.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Boxed inbound [`RemoteMediaTrack`] passed across the connection/callback boundary. `Send + Sync`
/// off the browser; single-threaded on it (same split as
/// [`BoxedTransportCallback`](crate::core::callback)).
#[cfg(not(feature = "web-sys-webrtc"))]
pub type BoxedRemoteMediaTrack = Box<dyn RemoteMediaTrack + Send + Sync>;

/// Boxed inbound [`RemoteMediaTrack`] passed across the connection/callback boundary.
#[cfg(feature = "web-sys-webrtc")]
pub type BoxedRemoteMediaTrack = Box<dyn RemoteMediaTrack>;

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

impl ChannelConfig {
    /// Check that this connection admits a local track of `kind` before it is attached. The contract
    /// is identical on every backend: a connection accepts media only when it was created with a
    /// media channel ([`MediaError::Unsupported`] otherwise), and the track's kind must match the
    /// negotiated one ([`MediaError::KindMismatch`]). Backends call this from `add_media_track` so
    /// the same `ChannelConfig` means the same thing on native and browser.
    pub fn admit_local_track(&self, kind: MediaKind) -> Result<&MediaChannelConfig, MediaError> {
        let media = self.media.as_ref().ok_or(MediaError::Unsupported)?;
        if media.kind != kind {
            return Err(MediaError::KindMismatch {
                expected: media.kind,
                got: kind,
            });
        }
        Ok(media)
    }
}

/// Errors from the media track path. Kept separate from a connection's `Error` so the trait's media
/// methods can carry a meaningful default ("unsupported") without every backend implementing them.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// This connection/backend has no media support (data-only, or a backend without media).
    #[error("media is not available on this connection")]
    Unsupported,
    /// The track's kind does not match the media kind this connection negotiated.
    #[error("media kind mismatch: connection negotiated {expected:?}, track is {got:?}")]
    KindMismatch {
        /// The kind the connection's [`ChannelConfig`] negotiated.
        expected: MediaKind,
        /// The kind of the track passed to `add_media_track`.
        got: MediaKind,
    },
    /// The underlying peer connection rejected the track.
    #[error("failed to add media track: {0}")]
    AddTrack(String),
}
