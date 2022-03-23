use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use async_trait::async_trait;
use web3::types::Address;

#[derive(Debug)]
pub enum Event {
    ConnectFailed,
    DirectMessage(ChannelMessage),
    WrapperMessage(Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelMessage {
    Ping(Address),
    Pong(Address),
    OtherChannelMessage
}

#[async_trait]
pub trait Channel<T: Send> {
    type Sender;
    type Receiver;

    fn new(buffer: usize) -> Self;
    fn sender(&self) -> Self::Sender;
    fn receiver(&self) -> Self::Receiver;
    async fn send(sender: &Self::Sender, msg: T) -> Result<()>;
    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>>;
}
