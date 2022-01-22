use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportBuilder;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use js_sys::Reflect;
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::unimplemented;
use wasm_bindgen::prelude::*;

use wasm_bindgen_futures::JsFuture;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::RtcIceCandidate;

use web_sys::RtcPeerConnection;

#[derive(Clone)]
pub struct WasmTransport {
    pub connection: Option<Arc<RtcPeerConnection>>,
    pub offer: Option<String>,
    pub channel: Option<Arc<RtcDataChannel>>,
}

unsafe impl Sync for WasmTransport {}

#[async_trait(?Send)]
impl IceTransport for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = RtcIceCandidate;
    type Sdp = String;
    type Channel = RtcDataChannel;
    type ConnectionState = String;

    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>> {
        return self.connection.as_ref().map(|c| Arc::clone(c));
    }

    async fn get_pending_candidates(&self) -> Vec<Self::Candidate> {
        unimplemented!();
    }

    async fn get_answer(&self) -> Result<Self::Sdp> {
        unimplemented!();
    }

    async fn get_offer(&self) -> Result<Self::Sdp> {
        match &self.offer {
            Some(o) => Ok(o.to_string()),
            None => Err(anyhow!("Cannot get Offer")),
        }
    }

    async fn get_data_channel(&self, _label: &str) -> Result<Arc<Self::Channel>> {
        match &self.channel {
            Some(c) => Ok(c.to_owned()),
            None => Err(anyhow!("Faied to get channel")),
        }
    }

    async fn set_local_description<T>(&self, _desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + std::marker::Send,
    {
        unimplemented!();
    }

    async fn set_remote_description<T>(&self, _desc: T) -> Result<()>
    where
        T: Into<Self::Sdp> + std::marker::Send,
    {
        unimplemented!();
    }

    async fn on_ice_candidate(
        &self,
        _f: Box<
            dyn FnMut(Option<Self::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        unimplemented!();
    }

    async fn on_peer_connection_state_change(
        &self,
        _f: Box<
            dyn FnMut(Self::ConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        unimplemented!();
    }

    async fn on_data_channel(
        &self,
        _f: Box<
            dyn FnMut(Arc<Self::Channel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        unimplemented!();
    }
}

#[async_trait(?Send)]
impl IceTransportBuilder for WasmTransport {
    fn new() -> Self {
        let mut config = RtcConfiguration::new();
        config.ice_servers(
            &JsValue::from_serde(&json! {[{"urls":"stun:stun.l.google.com:19302"}]}).unwrap(),
        );

        let ins = Self {
            connection: RtcPeerConnection::new_with_configuration(&config)
                .ok()
                .as_ref()
                .map(|c| Arc::new(c.to_owned())),
            channel: None,
            offer: None,
        };
        return ins;
    }

    async fn start(&mut self) -> Result<()> {
        self.setup_offer().await;
        self.setup_channel("bns").await;
        return Ok(());
    }
}
impl WasmTransport {
    pub async fn setup_offer(&mut self) -> &Self {
        if let Some(connection) = &self.connection {
            if let Ok(offer) = JsFuture::from(connection.create_offer()).await {
                self.offer = Reflect::get(&offer, &JsValue::from_str("sdp"))
                    .ok()
                    .and_then(|o| o.as_string())
                    .take();
            }
        }
        return self;
    }

    pub async fn setup_channel(&mut self, name: &str) -> &Self {
        if let Some(conn) = &self.connection {
            let channel = conn.create_data_channel(&name);
            self.channel = Some(Arc::new(channel));
        }
        return self;
    }
}
