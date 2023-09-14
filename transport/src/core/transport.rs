//! The main entity of this module is the [ConnectionInterface] trait, which provides an
//! interface for establishing connections with other nodes, send data channel message to it.
//!
//! There is also a [ConnectionCreation] trait, which is used to specifies the creation of a
//! [ConnectionInterface] object for [Transport](crate::Transport).

use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::core::callback::BoxedCallback;

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

/// The [ConnectionInterface](transport::ConnectionInterface) trait defines how to
/// make webrtc ice handshake with a remote peer and then send data channel message to it.
#[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
#[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
pub trait ConnectionInterface {
    /// Sdp is used to expose local and remote session descriptions when handshaking.
    type Sdp: Serialize + DeserializeOwned;
    /// The error type that is returned by connection.
    type Error: std::error::Error;

    /// Send a [TransportMessage] to the remote peer.
    async fn send_message(&self, msg: TransportMessage) -> Result<(), Self::Error>;

    /// Get current webrtc connection state.
    fn webrtc_connection_state(&self) -> WebrtcConnectionState;

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

    /// Deprecated, should use `webrtc_connection_state`.
    fn ice_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_connection_state()
    }

    /// Deprecated, should check the state of `webrtc_connection_state`.
    async fn is_connected(&self) -> bool {
        self.webrtc_connection_state() == WebrtcConnectionState::Connected
    }

    /// Deprecated, should check the state of `webrtc_connection_state`.
    async fn is_disconnected(&self) -> bool {
        matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Disconnected
                | WebrtcConnectionState::Failed
                | WebrtcConnectionState::Closed
        )
    }
}

/// This trait specifies the creation of a [ConnectionInterface] object for [Transport](crate::Transport).
/// Each platform must implement this trait for its own connection implementation.
/// See [connections](crate::connections) module for examples.
#[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
#[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
pub trait ConnectionCreation {
    /// The connection type that is created by this trait.
    type Connection: ConnectionInterface<Error = Self::Error>;
    /// The error type that is returned by transport.
    type Error: std::error::Error;

    /// Used to create a new connection and register it in [Transport](crate::Transport).
    ///
    /// To avoid memory leak, this function will not return a connection object.
    /// Instead, use should use `get_connection` method of [Transport](crate::Transport)
    /// to get a [ConnectionRef](crate::connection_ref::ConnectionRef) after creation.
    ///
    /// See [connections](crate::connections) module for examples.
    async fn new_connection(
        &self,
        cid: &str,
        callback: Arc<BoxedCallback>,
    ) -> Result<(), Self::Error>;
}
