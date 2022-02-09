use crate::types::channel::Channel;
use crate::types::channel::Events;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct TkChannel {
    sender: Arc<mpsc::Sender<Events>>,
    receiver: mpsc::Receiver<Events>,
}

#[async_trait(?Send)]
impl Channel for TkChannel {
    type Sender = Arc<mpsc::Sender<Events>>;
    type Receiver = mpsc::Receiver<Events>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = mpsc::channel::<Events>(buffer);
        Self {
            sender: Arc::new(tx),
            receiver: rx,
        }
    }

    fn sender(&self) -> Self::Sender {
        Arc::clone(&self.sender)
    }

    async fn send(&self, e: Events) -> Result<()> {
        return Ok(self.sender.send(e).await?);
    }

    async fn recv(&mut self) {
        while let Some(e) = self.receiver.recv().await {
            _ = self.handler(e).await;
        }
    }

    async fn handler(&self, e: Events) {
        match e {
            Events::Null => (),
            Events::ConnectFailed => {
                println!("ConnectFailed");
            }
            _ => (),
        }
    }
}
