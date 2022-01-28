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
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::MessageEvent;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::RtcDataChannelEvent;
use web_sys::RtcIceCandidate;
use web_sys::RtcPeerConnection;
use web_sys::RtcPeerConnectionIceEvent;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;
use web_sys::RtcIceConnectionState;

#[derive(Clone)]
pub struct WasmTransport {
    pub connection: Option<Arc<RtcPeerConnection>>,
    pub offer: Option<RtcSessionDescription>,
    pub channel: Option<Arc<RtcDataChannel>>,
    pub pending_candidates: Arc<Vec<RtcIceCandidate>>,
}

#[async_trait(?Send)]
impl IceTransport for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = RtcIceCandidate;
    type Sdp = RtcSessionDescription;
    type Channel = RtcDataChannel;
    type ConnectionState = RtcIceConnectionState;
    type Msg = JsValue;

    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>> {
        self.connection.as_ref().map(|c| Arc::clone(c))
    }

    async fn get_pending_candidates(&self) -> Vec<Self::Candidate> {
        self.pending_candidates.to_vec()
    }

    async fn get_answer(&self) -> Result<Self::Sdp> {
        match self.get_peer_connection().await {
            Some(c) => {
                let promise = c.create_answer();
                match JsFuture::from(promise).await {
                    Ok(answer) => Ok(answer.into()),
                    Err(_) => Err(anyhow!("Failed to set remote description")),
                }
            },
            None => Err(anyhow!("cannot get answer")),
        }
    }

    async fn get_offer(&self) -> Result<Self::Sdp> {
        match &self.offer {
            Some(o) => Ok(o.clone()),
            None => Err(anyhow!("Cannot get Offer")),
        }
    }

    async fn get_data_channel(&self) -> Option<Arc<Self::Channel>> {
        self.channel.as_ref().map(|c| Arc::clone(&c))
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
        f: Box<
            dyn FnMut(Self::ConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_peer_connection().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: RtcIceConnectionState| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev).await })
                }) as Box<dyn FnMut(RtcIceConnectionState)>);
                c.set_oniceconnectionstatechange(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_data_channel(
        &self,
        f: Box<
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
                    spawn_local(async move { f(Arc::new(ev.channel())).await })
                })
                    as Box<dyn FnMut(RtcDataChannelEvent)>);
                c.set_ondatachannel(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_message(
        &self,
        f: Box<
            dyn FnMut(Self::Msg) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_data_channel().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: MessageEvent| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev.data()).await })
                }) as Box<dyn FnMut(MessageEvent)>);
                c.set_onmessage(Some(callback.as_ref().unchecked_ref()));
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
            pending_candidates: Arc::new(vec![]),
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
