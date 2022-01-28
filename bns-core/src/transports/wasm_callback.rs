use log::info;

use web_sys::RtcDataChannel;
use web_sys::{MessageEvent, RtcDataChannelEvent, RtcPeerConnectionIceEvent};

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
