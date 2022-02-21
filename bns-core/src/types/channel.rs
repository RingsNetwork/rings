use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug)]
pub enum Events {
    Null,
    ConnectFailed,
    SendMsg(Vec<u8>),
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
    async fn recv(&self) -> ();
    async fn handler(&self, e: Events) -> ();
}
