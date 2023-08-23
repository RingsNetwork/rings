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

#[async_trait]
pub trait SharedConnection: Clone + Send + Sync + 'static {
    type Sdp: Serialize + DeserializeOwned + Send + Sync;
    type Error: std::error::Error;
    type IceConnectionState: std::fmt::Debug;

    async fn is_connected(&self) -> bool;
    async fn is_disconnected(&self) -> bool;
    async fn send_message(&self, msg: TransportMessage) -> Result<(), Self::Error>;

    fn ice_connection_state(&self) -> Self::IceConnectionState;
    async fn webrtc_create_offer(&self) -> Result<Self::Sdp, Self::Error>;
    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp, Self::Error>;
    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<(), Self::Error>;
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
