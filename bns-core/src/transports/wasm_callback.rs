use crate::channels::wasm::CbChannel;
use crate::transports::wasm::WasmTransport;
use crate::types::channel::Channel;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use log::info;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use web_sys::RtcDataChannel;
use web_sys::{MessageEvent, RtcDataChannelEvent, RtcPeerConnectionIceEvent};

struct WasmCallback {
    channel: CbChannel,
}

impl WasmCallback {
    pub async fn on_message(
        &self,
    ) -> Box<
        dyn FnMut(
                <WasmTransport as IceTransport>::Msg,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        let sender = self.channel.sender();
        box move |msg: <WasmTransport as IceTransport>::Msg| {
            let sender = Arc::clone(&sender);
            let msg = msg.as_string().unwrap().clone();
            Box::pin(async move {
                info!("{:?}", msg);
                sender.send(Events::ReceiveMsg(msg)).unwrap();
            })
        }
    }
}
