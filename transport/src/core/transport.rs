use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::core::callback::BoxedCallback;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TransportMessage {
    Custom(Vec<u8>),
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum WebrtcConnectionState {
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

#[async_trait]
pub trait SharedConnection: Clone + Send + Sync + 'static {
    type Sdp: Serialize + DeserializeOwned + Send + Sync;
    type Error: std::error::Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<(), Self::Error>;

    fn webrtc_connection_state(&self) -> WebrtcConnectionState;
    async fn webrtc_create_offer(&self) -> Result<Self::Sdp, Self::Error>;
    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp, Self::Error>;
    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<(), Self::Error>;

    // TODO: deprecated, should use webrtc_connection_state
    fn ice_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_connection_state()
    }

    // TODO: deprecated, should check the state of webrtc_connection_state
    async fn is_connected(&self) -> bool {
        self.webrtc_connection_state() == WebrtcConnectionState::Connected
    }

    // TODO: deprecated, should check the state of webrtc_connection_state
    async fn is_disconnected(&self) -> bool {
        matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Disconnected
                | WebrtcConnectionState::Failed
                | WebrtcConnectionState::Closed
        )
    }
}

#[async_trait]
pub trait SharedTransport: Clone + Send + Sync + 'static {
    type Connection: SharedConnection<Error = Self::Error>;
    type Error: std::error::Error + Send + Sync + 'static;

    async fn new_connection<CE>(
        &self,
        cid: &str,
        callback: Arc<BoxedCallback<CE>>,
    ) -> Result<Self::Connection, Self::Error>
    where
        CE: std::error::Error + Send + Sync + 'static;

    fn get_connection(&self, cid: &str) -> Result<Self::Connection, Self::Error>;
    fn get_connections(&self) -> Vec<(String, Self::Connection)>;
    fn get_connection_ids(&self) -> Vec<String>;

    async fn close_connection(&self, cid: &str) -> Result<(), Self::Error>;

    async fn send_message(&self, cid: &str, msg: TransportMessage) -> Result<(), Self::Error> {
        self.get_connection(cid)?.send_message(msg).await
    }
}
