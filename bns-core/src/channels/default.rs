use crate::types::channel::Channel;
use crate::types::channel::Events;
use anyhow::Result;
use async_channel as ac;
use async_channel::Receiver;
use async_channel::Sender;
use async_trait::async_trait;

pub struct AcChannel {
    sender: Sender<Events>,
    receiver: Receiver<Events>,
}

#[async_trait]
impl Channel for AcChannel {
    type Sender = Sender<Events>;
    type Receiver = Receiver<Events>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = ac::bounded(buffer);
        Self {
            sender: tx,
            receiver: rx,
        }
    }

    fn sender(&self) -> Self::Sender {
        self.sender.clone()
    }

    fn receiver(&self) -> Self::Receiver {
        self.receiver.clone()
    }

    async fn send(&self, e: Events) -> Result<()> {
        Ok(self.sender.send(e).await?)
    }

    async fn recv(&self) -> Result<Events> {
        self.receiver().recv().await.map_err(|e| anyhow::anyhow!(e))
    }
}
