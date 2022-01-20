use anyhow::Result;
use async_trait::async_trait;
use std::unimplemented;
use js_sys::Reflect;
use tokio::sync::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use serde_json::json;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::{MessageEvent, RtcDataChannelEvent, RtcPeerConnection, RtcPeerConnectionIceEvent};
use web_sys::{RtcSdpType, RtcSessionDescriptionInit};
use crate::types::ice_transport::IceTransport;

#[derive(Clone)]
pub struct WasmTransport {
    pub connection: Arc<Mutex<RtcPeerConnection>>,
    pub channel: Arc<Mutex<Vec<RtcDataChannel>>>,
}


#[async_trait]
impl IceTransport for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = String;
    type Sdp = String;
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
