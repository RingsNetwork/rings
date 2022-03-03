use crate::dht::chord::ChordAction;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use serde_json;
use std::convert::TryFrom;

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

impl TryFrom<ChordAction> for Events {
    type Error = anyhow::Error;

    fn try_from(input: ChordAction) -> Result<Self> {
        let result = serde_json::to_vec(&input).map_err(|e| anyhow!(e))?;
        Ok(Events::SendMsg(result))
    }
}
