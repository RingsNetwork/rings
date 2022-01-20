use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::offer_answer_options::RTCAnswerOptions;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use webrtc::peer_connection::RTCPeerConnection;

#[async_trait]
pub trait IceTransport {
    type Connection;
    type Candidate;
    type Sdp;
    type Channel;
    type ConnectionState;

    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>>;
    async fn get_pending_candidates(&self) -> Arc<Mutex<Vec<Self::Candidate>>>;
    async fn get_answer(&self) -> Result<Self::Sdp>;
    async fn get_offer(&self) -> Result<Self::Sdp>;

    async fn get_data_channel(
        &self,
        label: &str,
        options: Option<RTCDataChannelInit>,
    ) -> Result<Arc<Mutex<Arc<RTCDataChannel>>>>;

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + std::marker::Send;
    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + std::marker::Send;
    async fn on_ice_candidate(
        &self,
        f: Box<
            dyn FnMut(Option<Self::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()>;
    async fn on_peer_connection_state_change(
        &self,
        f: Box<
            dyn FnMut(Self::ConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()>;
    async fn on_data_channel(
        &self,
        f: Box<
            dyn FnMut(Arc<Self::Channel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()>;
}
