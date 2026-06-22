//! The main entity of this module is the [ConnectionInterface] trait, which provides an
//! interface for establishing connections with other nodes, send data channel message to it.
//!
//! There is also a [TransportInterface] trait, which is used to specify the management of all
//! [ConnectionInterface] objects.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::media::MediaError;
use crate::core::sdp::parse_sdp_max_message_size;
use crate::delivery::DeliveryFuture;

/// Wrapper for the data that is sent over the data channel.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TransportMessage {
    /// The custom message is sent by an external invoker and
    /// should be handled by the on_message callback.
    Custom(Vec<u8>),
}

/// The state of the WebRTC connection.
/// This enum is used to define a same interface for all the platforms.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum WebrtcConnectionState {
    /// Unspecified
    #[default]
    Unspecified,

    /// WebrtcConnectionState::New indicates that any of the ICETransports or
    /// DTLSTransports are in the "new" state and none of the transports are
    /// in the "connecting", "checking", "failed" or "disconnected" state, or
    /// all transports are in the "closed" state, or there are no transports.
    New,

    /// WebrtcConnectionState::Connecting indicates that any of the
    /// ICETransports or DTLSTransports are in the "connecting" or
    /// "checking" state and none of them is in the "failed" state.
    Connecting,

    /// WebrtcConnectionState::Connected indicates that all ICETransports and
    /// DTLSTransports are in the "connected", "completed" or "closed" state
    /// and at least one of them is in the "connected" or "completed" state.
    Connected,

    /// WebrtcConnectionState::Disconnected indicates that any of the
    /// ICETransports or DTLSTransports are in the "disconnected" state
    /// and none of them are in the "failed" or "connecting" or "checking" state.
    Disconnected,

    /// WebrtcConnectionState::Failed indicates that any of the ICETransports
    /// or DTLSTransports are in a "failed" state.
    Failed,

    /// WebrtcConnectionState::Closed indicates the peer connection is closed
    /// and the isClosed member variable of PeerConnection is true.
    Closed,
}

/// Interop ceiling for a single data-channel message, in bytes. webrtc-rs (native) enforces this
/// SCTP `max_message_size` on send, and it is the value a peer can be relied on to accept — so a
/// sender must never exceed it regardless of what the remote advertises. A per-channel
/// [`max_message_size`](ConnectionInterface::max_message_size) may resolve to *less* (a constrained
/// peer) but never more.
pub const MAX_DATA_CHANNEL_MESSAGE_SIZE: usize = 65536;

/// The effective per-message send limit for a peer whose SDP is `remote_sdp`. The negotiated value
/// is parsed from the SDP by [`crate::core::sdp`]; this function is the *policy* layered on top.
/// Per RFC 8841 an absent attribute defaults to 65536 and a value of `0` means "no limit" (we still
/// bound it by our own send cap); any explicit value is honoured but capped at
/// [`MAX_DATA_CHANNEL_MESSAGE_SIZE`] for interop. Always returns a positive value.
pub fn effective_max_message_size(remote_sdp: &str) -> usize {
    match parse_sdp_max_message_size(remote_sdp) {
        None | Some(0) => MAX_DATA_CHANNEL_MESSAGE_SIZE,
        Some(n) => (n as usize).min(MAX_DATA_CHANNEL_MESSAGE_SIZE),
    }
}

/// The [ConnectionInterface] trait defines how to
/// make webrtc ice handshake with a remote peer and then send data channel message to it.
#[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
#[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
pub trait ConnectionInterface {
    /// Sdp is used to expose local and remote session descriptions when handshaking.
    type Sdp: Serialize + DeserializeOwned;
    /// The error type that is returned by connection.
    type Error: std::error::Error;
    /// The backend's own local-track type accepted by [`add_media_track`](Self::add_media_track).
    /// Typing it per backend keeps the outbound media path free of trait-object downcasts: a native
    /// connection takes a `NativeMediaTrack`, a browser one a `BrowserMediaTrack`, and a backend
    /// without media support sets this to `()`. The construction of a local track is platform-
    /// specific by nature (see the concrete [`MediaTrack`](crate::core::media::MediaTrack) types).
    ///
    /// Off the browser the default `add_media_track` future must be `Send`, so the track is too;
    /// browser tracks (`MediaStreamTrack`) are not `Send`, matching that backend's single-threaded
    /// `async_trait(?Send)`.
    #[cfg(not(feature = "web-sys-webrtc"))]
    type LocalMediaTrack: Send;
    /// The backend's own local-track type accepted by [`add_media_track`](Self::add_media_track).
    #[cfg(feature = "web-sys-webrtc")]
    type LocalMediaTrack;

    /// Send a [TransportMessage] to the remote peer.
    ///
    /// The returned `Result` reflects whether the bytes were accepted into the
    /// local send buffer. The [DeliveryFuture] it yields resolves later to the
    /// message's actual fate: `Ok(())` once flushed to the wire, or `Err(..)`
    /// if the channel closed while the bytes were still buffered. Callers that
    /// don't care can drop it; callers that do can spawn it (see
    /// [crate::delivery]).
    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture, Self::Error>;

    /// Get current webrtc connection state.
    fn webrtc_connection_state(&self) -> WebrtcConnectionState;

    /// The maximum size, in bytes, of one message this connection can send — the channel's
    /// negotiated SCTP / data-channel `max_message_size`, capped at
    /// [`MAX_DATA_CHANNEL_MESSAGE_SIZE`] for cross-peer interop. A caller must keep every sent
    /// message at or below this; larger payloads have to be chunked. Reported per-channel so a
    /// constrained channel (which can negotiate a smaller limit) is respected.
    fn max_message_size(&self) -> usize;

    /// Attach a local media track to this connection, to be sent to the peer. Returns the attached
    /// track's id, so the caller can [`remove_media_track`](Self::remove_media_track) it again if a
    /// later step of the same operation (e.g. the renegotiation offer) fails — keeping "add a track
    /// and renegotiate" a transaction that leaves no half-attached track behind.
    ///
    /// Defaults to [`MediaError::Unsupported`]: a connection only carries media when it was created
    /// with a [`media`](crate::core::media::ChannelConfig) channel, and only the backends that
    /// support media override this. The parameter is the backend's own
    /// [`LocalMediaTrack`](Self::LocalMediaTrack) type, so there is no downcast on this path.
    async fn add_media_track(&self, _track: Self::LocalMediaTrack) -> Result<String, MediaError> {
        Err(MediaError::Unsupported)
    }

    /// Detach a previously [`add_media_track`](Self::add_media_track)ed track by its id, undoing the
    /// `PeerConnection` sender/track mutation. Used to roll back a renegotiation that attached the
    /// track but could not complete (so the connection returns to exactly its prior state). Defaults
    /// to a no-op for backends without media; removing an unknown id is also a no-op.
    async fn remove_media_track(&self, _track_id: &str) -> Result<(), MediaError> {
        Ok(())
    }

    /// This is a debug method to dump the stats of webrtc connection.
    async fn get_stats(&self) -> Vec<String>;

    /// Create a webrtc offer to start handshake.
    async fn webrtc_create_offer(&self) -> Result<Self::Sdp, Self::Error>;

    /// Accept a webrtc offer from remote peer and give back an answer.
    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp, Self::Error>;

    /// Accept a webrtc answer from remote peer.
    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<(), Self::Error>;

    /// Wait for the data channel to be opened after handshake.
    async fn webrtc_wait_for_data_channel_open(&self) -> Result<(), Self::Error>;

    /// Close the webrtc connection.
    async fn close(&self) -> Result<(), Self::Error>;
}

/// This trait specifies how to management [ConnectionInterface] objects.
/// Each platform must implement this trait for its own connection implementation.
/// See [connections](crate::connections) module for examples.
#[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
#[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
pub trait TransportInterface {
    /// The connection type that is created by this trait.
    type Connection: ConnectionInterface<Error = Self::Error>;

    /// The error type that is returned by transport.
    type Error: std::error::Error;

    /// Used to create a new connection and register it in the transport.
    ///
    /// To avoid memory leak, this function will not return a connection object.
    /// Instead, user should use `connection` method of to get a [ConnectionRef](crate::connection_ref::ConnectionRef)
    /// after creation.
    ///
    /// See [connections](crate::connections) module for examples.
    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<(), Self::Error>;

    /// This method closes and releases the connection from transport.
    /// All references to this cid, created by `get_connection`, will be released.
    async fn close_connection(&self, cid: &str) -> Result<(), Self::Error>;

    /// Get a reference of the connection by its id.
    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>, Self::Error>;

    /// Get all the connections in the transport.
    fn connections(&self) -> Vec<(String, ConnectionRef<Self::Connection>)>;

    /// Get all the connection ids in the transport.
    fn connection_ids(&self) -> Vec<String>;
}

/// Used to store a boxed [TransportInterface] trait object.
#[cfg(not(feature = "web-sys-webrtc"))]
pub type BoxedTransport<C, E> =
    Box<dyn TransportInterface<Connection = C, Error = E> + Send + Sync>;

/// Used to store a boxed [TransportInterface] trait object.
#[cfg(feature = "web-sys-webrtc")]
pub type BoxedTransport<C, E> = Box<dyn TransportInterface<Connection = C, Error = E>>;

#[cfg(test)]
mod tests {
    // SDP parsing (including section semantics) is tested in `crate::core::sdp`; these cover the
    // policy `effective_*` layers on top of it (default / no-limit / cap).
    use super::effective_max_message_size;
    use super::MAX_DATA_CHANNEL_MESSAGE_SIZE;

    /// A data-channel SDP advertising `max-message-size:<value>` in the right media section.
    fn sdp_with(value: &str) -> String {
        format!(
            "v=0\r\n\
             m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
             a=max-message-size:{value}\r\n"
        )
    }

    #[test]
    fn effective_absent_defaults_to_cap() {
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert_eq!(
            effective_max_message_size(sdp),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_zero_means_no_limit_uses_cap() {
        assert_eq!(
            effective_max_message_size(&sdp_with("0")),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_smaller_value_is_honoured() {
        assert_eq!(effective_max_message_size(&sdp_with("16384")), 16384);
    }

    #[test]
    fn effective_larger_value_is_capped() {
        assert_eq!(
            effective_max_message_size(&sdp_with("1048576")),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_exactly_cap_is_cap() {
        assert_eq!(
            effective_max_message_size(&sdp_with("65536")),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }
}
