pub mod ice_server;
pub use self::ice_server::IceServer;
use crate::ecc::PublicKey;
use crate::err::Result;
use crate::message::Encoded;
use crate::session::SessionManager;
use crate::types::channel::Channel;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use web3::types::Address;

/// Struct From [webrtc-rs](https://docs.rs/webrtc/latest/webrtc/ice_transport/ice_candidate/struct.RTCIceCandidateInit.html)
/// For [RFC Std](https://w3c.github.io/webrtc-pc/#dom-rtcicecandidate-tojson), ICE Candidate should be camelCase
/// dictionary RTCIceCandidateInit {
///  DOMString candidate = "";
///  DOMString? sdpMid = null;
///  unsigned short? sdpMLineIndex = null;
///  DOMString? usernameFragment = null;
/// };
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
    pub username_fragment: Option<String>,
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceTransport<E: Send, Ch: Channel<E>> {
    type Connection;
    type Candidate;
    type Sdp;
    type DataChannel;
    type IceConnectionState;
    type Msg;

    fn new(event_sender: Ch::Sender) -> Self;
    async fn start(&mut self, addr: &IceServer) -> Result<&Self>;
    async fn close(&self) -> Result<()>;
    async fn ice_connection_state(&self) -> Option<Self::IceConnectionState>;
    async fn is_connected(&self) -> bool;
    async fn pubkey(&self) -> PublicKey;
    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>>;
    async fn get_pending_candidates(&self) -> Vec<Self::Candidate>;
    async fn get_answer(&self) -> Result<Self::Sdp>;
    async fn get_offer(&self) -> Result<Self::Sdp>;
    async fn get_answer_str(&self) -> Result<String>;
    async fn get_offer_str(&self) -> Result<String>;
    async fn get_data_channel(&self) -> Option<Arc<Self::DataChannel>>;
    async fn send_message(&self, msg: &Vec<u8>) -> Result<()>;
    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + Send;
    async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()>;
    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + Send;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceTransportCallback<E: Send, Ch: Channel<E>>: IceTransport<E, Ch> {
    type OnLocalCandidateHdlrFn;
    type OnDataChannelHdlrFn;
    type OnIceConnectionStateChangeHdlrFn;
    async fn apply_callback(&self) -> Result<&Self>;
    async fn on_ice_connection_state_change(&self) -> Self::OnIceConnectionStateChangeHdlrFn;
    async fn on_ice_candidate(&self) -> Self::OnLocalCandidateHdlrFn;
    async fn on_data_channel(&self) -> Self::OnDataChannelHdlrFn;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceTrickleScheme<E: Send, Ch: Channel<E>>: IceTransport<E, Ch> {
    type SdpType;
    async fn get_handshake_info(
        &self,
        session: SessionManager,
        kind: Self::SdpType,
    ) -> Result<Encoded>;
    async fn register_remote_info(&self, data: Encoded) -> Result<Address>;
    async fn wait_for_connected(&self) -> Result<()>;
}
