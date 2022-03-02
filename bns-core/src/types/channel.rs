use crate::dht::chord::ChordAction;
use anyhow::Result;
use async_trait::async_trait;
use serde_json;

#[derive(Debug)]
pub enum Events {
    Null,
    ConnectFailed,
    ReceiveMsg(Vec<u8>),
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait Channel {
    type Sender;
    type Receiver;

    fn new(buffer: usize) -> Self;
    fn sender(&self) -> Self::Sender;
    fn receiver(&self) -> Self::Receiver;
    async fn send(&self, e: Events) -> Result<()>;
    async fn recv(&self) -> Result<Events>;
}

// impl Serialize & Deserialize ChordAction to Vec<u8>
//
impl Events {
    pub fn from_action(action: ChordAction) -> Events {
        let result = serde_json::to_vec(&action);
        match result {
            Ok(msg) => Events::SendMsg(msg),
            Err(e) => {
                panic!("from action {:?} raise exception {:?}", action, e)
            }
        }
    }
}
