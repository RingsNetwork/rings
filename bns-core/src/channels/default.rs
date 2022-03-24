use crate::types::channel::Channel;
use anyhow::Result;
use async_channel as ac;
use async_channel::Receiver;
use async_channel::Sender;
use async_trait::async_trait;

pub struct AcChannel<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

#[async_trait]
impl<T: Send> Channel<T> for AcChannel<T> {
    type Sender = Sender<T>;
    type Receiver = Receiver<T>;

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

    async fn send(sender: &Self::Sender, msg: T) -> Result<()> {
        match sender.send(msg).await {
            Ok(_) => Ok(()),
            Err(_) => Err(anyhow::anyhow!("failed on sending message")),
        }
    }

    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>> {
        match receiver.recv().await {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    }
}
