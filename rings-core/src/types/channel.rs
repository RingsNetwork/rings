use crate::err::Result;
use async_trait::async_trait;
use serde::Serialize;
use web3::types::Address;

#[derive(Debug, PartialEq, Serialize, Clone)]
pub enum Event {
    ConnectFailed(Address),
    DataChannelMessage(Vec<u8>),
    RegisterTransport(Address),
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
