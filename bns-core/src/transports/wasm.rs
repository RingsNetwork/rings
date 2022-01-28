use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportBuilder;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use js_sys::Reflect;
use log::info;
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::unimplemented;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::RtcIceCandidate;
use web_sys::RtcPeerConnection;
use web_sys::RtcPeerConnectionIceEvent;
use web_sys::RtcDataChannelEvent;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;


#[derive(Clone)]
pub struct WasmTransport {
    pub connection: Option<Arc<RtcPeerConnection>>,
    pub offer: Option<RtcSessionDescription>,
    pub channel: Option<Arc<RtcDataChannel>>,
}

#[async_trait(?Send)]
impl IceTransport for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = RtcIceCandidate;
    type Sdp = RtcSessionDescription;
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
            Some(o) => Ok(o.clone()),
            None => Err(anyhow!("Cannot get Offer")),
        }
    }

    async fn get_data_channel(&self) -> Result<Arc<Self::Channel>> {
        match &self.channel {
            Some(c) => Ok(c.to_owned()),
            None => Err(anyhow!("Faied to get channel")),
        }
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp>,
    {
        match &self.get_peer_connection().await {
            Some(c) => {
                let mut offer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
                offer_obj.sdp(&desc.into().sdp());
                let promise = c.set_local_description(&offer_obj);
                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(_) => Err(anyhow!("Failed to set remote description")),
                }
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp>,
    {
        match &self.get_peer_connection().await {
            Some(c) => {
                let mut offer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
                offer_obj.sdp(&desc.into().sdp());
                let promise = c.set_remote_description(&offer_obj);
                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(_) => Err(anyhow!("Failed to set remote description")),
                }
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_ice_candidate(
        &self,
        f: Box<
            dyn FnMut(Option<Self::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_peer_connection().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: RtcPeerConnectionIceEvent| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev.candidate()).await })
                })
                    as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
                c.set_onicecandidate(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
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
        let mut f = Some(f);
        match &self.get_peer_connection().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: RtcDataChannelEvent| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev.candidate()).await })
                })
                    as Box<dyn FnMut(RtcDataChannelEvent)>);
                c.set_ondatachannel(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
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
        info!("started!");
        return Ok(());
    }
}

impl WasmTransport {
    pub async fn setup_offer(&mut self) -> &Self {
        if let Some(connection) = &self.connection {
            if let Ok(offer) = JsFuture::from(connection.create_offer()).await {
                self.offer = Reflect::get(&offer, &JsValue::from_str("sdp"))
                    .ok()
                    .and_then(|o| Some(RtcSessionDescription::from(o)))
                    .take();
                info!("{:?}", self.offer);
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
