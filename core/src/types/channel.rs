use async_trait::async_trait;
use serde::Serialize;

use crate::dht::Did;
use crate::err::Result;

/// Event message send and recv through Channel.
#[derive(Debug, PartialEq, Eq, Serialize, Clone)]
pub enum Event {
    ConnectClosed((Did, uuid::Uuid)),
    DataChannelMessage(Vec<u8>),
    RegisterTransport((Did, uuid::Uuid)),
}

/// Channel trant implement methods.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait Channel<T: Send> {
    type Sender;
    type Receiver;

    fn new() -> Self;
    fn sender(&self) -> Self::Sender;
    fn receiver(&self) -> Self::Receiver;
    async fn send(sender: &Self::Sender, msg: T) -> Result<()>;
    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>>;
}
