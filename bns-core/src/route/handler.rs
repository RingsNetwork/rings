#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Events;
use std::sync::Arc;
use webrtc::data_channel::RTCDataChannel;

pub async fn handle_send_msg(event: Events, channle: Arc<RTCDataChannel>, signaler: Arc<Channel>) {
    log::debug!("Event: {:?}", event);
    match event {
        Events::SendMsg(msg) => {}
        _ => {
            panic!("Unable handle other Message");
        }
    }
}

pub async fn handle_recv_msg(event: Events, channle: Arc<RTCDataChannel>, signaler: Arc<Channel>) {
    log::debug!("Event: {:?}", event);
    match event {
        Events::ReceiveMsg(msg) => {}
        _ => {
            panic!("Unable handle other Message");
        }
    }
}
