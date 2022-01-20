use js_sys::Reflect;
use serde_json::json;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::{MessageEvent, RtcDataChannelEvent, RtcPeerConnection, RtcPeerConnectionIceEvent};
use web_sys::{RtcSdpType, RtcSessionDescriptionInit};

#[derive(Clone)]
pub struct IceTransport {
    pub offer: Option<String>,
    pub peer: Option<RtcPeerConnection>,
    pub channel: Option<RtcDataChannel>,
}

impl IceTransport {
    pub fn new() -> Self {
        let mut config = RtcConfiguration::new();
        config.ice_servers(
            &JsValue::from_serde(&json! {[{"urls":"stun:stun.l.google.com:19302"}]}).unwrap(),
        );

        return Self {
            offer: None,
            peer: RtcPeerConnection::new_with_configuration(&config).ok(),
            channel: None,
        };
        // let onopen_callback = Closure::wrap(Self::on_open());

        // transport.peer.set_ondatachannel(Some(onopen_callback.as_ref().unchecked_ref()));
        //            link.send_message(P2pMsg::UpdateP2p((channel, sdp, Some(peer))));
    }

    pub async fn setup(&mut self) {
        self.setup_channel("bns").await;
        self.setup_offer().await;
    }

    pub async fn setup_channel(&mut self, name: &str) -> &Self {
        if let Some(peer) = &self.peer {
            let channel = peer.create_data_channel(&name);
            let onmessage_callback = Closure::wrap(Self::on_message(channel.clone()));
            channel.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
            self.channel = Some(channel);
        }
        return self;
    }

    pub async fn setup_offer(&mut self) -> &Self {
        if let Some(peer) = &self.peer {
            if let Ok(offer) = JsFuture::from(peer.create_offer()).await {
                self.offer = Reflect::get(&offer, &JsValue::from_str("sdp"))
                    .ok()
                    .and_then(|o| o.as_string())
                    .take();
            }
        }
        return self;
    }

    pub async fn dial(&self, offer: String) {
        let mut offer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        offer_obj.sdp(&offer);
        if let Some(peer) = &self.peer {
            let srd_promise = peer.set_remote_description(&offer_obj);
            match JsFuture::from(srd_promise).await {
                _ => {}
            }
        }
    }

    pub fn on_message(channel: RtcDataChannel) -> Box<dyn FnMut(MessageEvent)> {
        box move |ev: MessageEvent| match ev.data().as_string() {
            Some(message) => {
                channel.send_with_str("Pong from pc1.dc!").unwrap();
            }
            None => {}
        }
    }

    pub fn on_channel() -> Box<dyn FnMut(RtcDataChannelEvent)> {
        box move |ev: RtcDataChannelEvent| {
            let cnn = ev.channel();
            match cnn.send_with_str("Greeting!") {
                Ok(_d) => {}
                Err(_e) => {
                    panic!();
                }
            };
        }
    }

    pub fn on_open() -> Box<dyn FnMut(RtcDataChannelEvent)> {
        box move |ev: RtcDataChannelEvent| {
            let cnn = ev.channel();
            match cnn.send_with_str("Greeting!") {
                Ok(_d) => {}
                Err(_e) => {
                    panic!();
                }
            };
        }
    }

    pub fn on_icecandidate() -> Box<dyn FnMut(RtcPeerConnectionIceEvent)> {
        box move |ev: RtcPeerConnectionIceEvent| match ev.candidate() {
            Some(candidate) => {}
            None => {}
        }
    }
}
