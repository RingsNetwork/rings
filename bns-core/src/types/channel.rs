use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug)]
pub enum Events {
    Null,
    ConnectFailed
}

#[async_trait(?Send)]
pub trait Channel {
    type Sender;
    type Receiver;

    fn new(buffer: usize) -> Self;
    fn sender(&self) -> Self::Sender;
    async fn send(&self, e: Events) -> Result<()>;
    async fn recv(&mut self) -> ();
    async fn handler(&self, e: Events) -> ();
}
