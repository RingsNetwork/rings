pub mod ice_server;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use web3::types::Address;

pub use self::ice_server::IceServer;
use crate::ecc::PublicKey;
use crate::err::Result;
use crate::message::Encoded;
use crate::session::SessionManager;
use crate::types::channel::Channel;

/// Struct From [webrtc-rs](https://docs.rs/webrtc/latest/webrtc/ice_transport/ice_candidate/struct.RTCIceCandidateInit.html)
/// According to [RFC Std](https://w3c.github.io/webrtc-pc/#dom-rtcicecandidate-tojson), ICE Candidate should be camelCase
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
pub trait IceTransport {
    type Connection;
    type Candidate;
    type Sdp;
    type DataChannel;

    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>>;
    async fn get_pending_candidates(&self) -> Vec<Self::Candidate>;
    async fn get_answer(&self) -> Result<Self::Sdp>;
    async fn get_offer(&self) -> Result<Self::Sdp>;
    async fn get_data_channel(&self) -> Option<Arc<Self::DataChannel>>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceTransportInterface<E: Send, Ch: Channel<E>> {
    type IceConnectionState;

    fn new(event_sender: Ch::Sender) -> Self;
    async fn start(&mut self, addr: Vec<IceServer>, external_id: Option<String>) -> Result<&Self>;
    async fn apply_callback(&self) -> Result<&Self>;
    async fn close(&self) -> Result<()>;
    async fn ice_connection_state(&self) -> Option<Self::IceConnectionState>;
    async fn is_connected(&self) -> bool;
    async fn is_disconnected(&self) -> bool;
    async fn pubkey(&self) -> PublicKey;
    async fn send_message(&self, msg: &[u8]) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceTransportCallback: IceTransport {
    type OnLocalCandidateHdlrFn;
    type OnDataChannelHdlrFn;
    type OnIceConnectionStateChangeHdlrFn;

    async fn on_ice_connection_state_change(&self) -> Self::OnIceConnectionStateChangeHdlrFn;
    async fn on_ice_candidate(&self) -> Self::OnLocalCandidateHdlrFn;
    async fn on_data_channel(&self) -> Self::OnDataChannelHdlrFn;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceCandidateGathering: IceTransport {
    async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()>;
    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where T: Into<Self::Sdp> + Send;
    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where T: Into<Self::Sdp> + Send;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait IceTrickleScheme {
    type SdpType;

    async fn get_handshake_info(
        &self,
        session_manager: &SessionManager,
        kind: Self::SdpType,
    ) -> Result<Encoded>;
    async fn register_remote_info(&self, data: Encoded) -> Result<Address>;
    async fn wait_for_connected(&self) -> Result<()>;
}
