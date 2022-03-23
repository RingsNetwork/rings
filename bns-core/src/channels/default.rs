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
impl<T> Channel for AcChannel<T> {
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
}

pub async fn recv<T>(receiver: &Receiver<T>) -> Result<T> {
    receiver.recv().await.map_err(|e| anyhow::anyhow!(e))
}
