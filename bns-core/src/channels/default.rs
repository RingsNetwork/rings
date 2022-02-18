use crate::types::channel::Channel;
use crate::types::channel::Events;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use async_channel as ac;
use async_channel::Receiver;
use async_channel::Sender;



pub struct AcChannel {
    sender: Arc<Sender<Events>>,
    receiver: Arc<Receiver<Events>>,
}

#[async_trait(?Send)]
impl Channel for AcChannel {
    type Sender = Arc<Sender<Events>>;
    type Receiver = Arc<Receiver<Events>>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = ac::bounded(buffer);
        return Self {
            sender: Arc::new(tx),
            receiver: Arc::new(rx),
        };
    }

    fn sender(&self) -> Self::Sender {
        return Arc::clone(&self.sender);
    }

    fn receiver(&self) -> Self::Receiver {
        Arc::clone(&self.receiver)
    }

    async fn send(&self, e: Events) -> Result<()> {
        return Ok(self.sender.send(e).await?);
    }

    async fn recv(&self) {
        match self.receiver().recv().await {
            Ok(e) => {
                _ = self.handler(e).await;
            }
            Err(e) => {
                log::error!("failed on recv: {:?}", e);
            }
        }
    }
    async fn handler(&self, e: Events) {
        match e {
            Events::Null => (),
            Events::ConnectFailed => {
                log::info!("ConnectFailed");
            }
            _ => (),
        }
    }
}
