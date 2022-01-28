use js_sys::Reflect;
use log::info;
use serde_json::json;
use std::any::Any;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::{MessageEvent, RtcDataChannelEvent, RtcPeerConnection, RtcPeerConnectionIceEvent};
use web_sys::{RtcSdpType, RtcSessionDescriptionInit};

struct WasmCallback {}

impl WasmCallback {
    pub fn on_message(channel: RtcDataChannel) -> Box<dyn FnMut(MessageEvent)> {
        box move |ev: MessageEvent| match ev.data().as_string() {
            Some(message) => {
                info!("{:?}", message);
                channel.send_with_str("Pong from pc1.dc!").unwrap();
            }
            None => {}
        }
    }

    pub fn on_channel() -> Box<dyn FnMut(RtcDataChannelEvent)> {
        box move |ev: RtcDataChannelEvent| {
            let cnn = ev.channel();
            info!("onDataChannelEvent!");
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
            info!("channel Open!");
            let cnn = ev.channel();
            match cnn.send_with_str("Greeting!") {
                Ok(_d) => {}
                Err(_e) => {
                    panic!("cannot send greeting");
                }
            };
        }
    }

    pub fn on_icecandidate() -> Box<dyn FnMut(RtcPeerConnectionIceEvent)> {
        box move |ev: RtcPeerConnectionIceEvent| {
            info!("ice candidate");
            match ev.candidate() {
                Some(candidate) => {
                    info!("onicecandiate: {:#?}", candidate.candidate());
                }
                None => {}
            }
        }
    }
}
