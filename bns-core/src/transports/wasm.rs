use anyhow::Result;
use async_trait::async_trait;
use std::unimplemented;
use js_sys::Reflect;
use tokio::sync::Mutex;
use std::future::Future;
use std::pin::Pin;
use serde_json::json;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::{MessageEvent, RtcDataChannelEvent, RtcPeerConnection, RtcPeerConnectionIceEvent};
use web_sys::{RtcSdpType, RtcSessionDescriptionInit};
use web_sys::RtcIceCandidate;
use web_sys::RtcSessionDescription;
use crate::types::ice_transport::IceTransport;

#[derive(Clone)]
pub struct WasmTransport {
    pub connection: Option<RtcPeerConnection>,
    pub offer: Option<String>,
    pub channel: Option<RtcDataChannel>,
}

unsafe impl Sync for WasmTransport {}

#[async_trait]
impl IceTransport for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = RtcIceCandidate;
    type Sdp = RtcSessionDescription;
    type Channel = RtcDataChannel;
    type ConnectionState = String;

    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>> {
        unimplemented!();
    }

    async fn get_pending_candidates(&self) -> Arc<Mutex<Vec<Self::Candidate>>> {
        unimplemented!();
    }

    async fn get_answer(&self) -> Result<Self::Sdp> {
         unimplemented!();
    }

    async fn get_offer(&self) -> Result<Self::Sdp> {
        unimplemented!();
    }

    async fn get_data_channel(&self, label: &str) -> Result<Arc<Mutex<Arc<Self::Channel>>>>{
        unimplemented!();
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + std::marker::Send {
           unimplemented!();
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + std::marker::Send {
          unimplemented!();
    }


    async fn on_ice_candidate(
        &self,
        f: Box<
            dyn FnMut(Option<Self::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
         unimplemented!();
    }

    async fn on_peer_connection_state_change(
        &self,
        f: Box<
            dyn FnMut(Self::ConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        unimplemented!();

    }

    async fn on_data_channel(
        &self,
        f: Box<
            dyn FnMut(Arc<Self::Channel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
         unimplemented!();
    }


}

impl WasmTransport {
    pub fn new() -> Self {
        let mut config = RtcConfiguration::new();
        config.ice_servers(
            &JsValue::from_serde(&json! {[{"urls":"stun:stun.l.google.com:19302"}]}).unwrap(),
        );
        return Self {
            connection: RtcPeerConnection::new_with_configuration(&config).ok(),
            channel: None,
            offer: None
        };
    }

    pub async fn setup_offer(&mut self) -> &Self {
        if let Some(connection) = &self.connection {
            if let Ok(offer) = JsFuture::from(connection.create_offer()).await {
                self.offer = Reflect::get(&offer, &JsValue::from_str("sdp")).ok()
                    .and_then(|o| o.as_string())
                    .take();
            }
        }
        return self;
    }

    pub async fn setup_channel(&mut self, name: &str) -> &Self {
        if let Some(conn) = &self.connection {
            let channel = conn.create_data_channel(&name);
            self.channel = Some(channel);
        }
        return self;
    }
}
