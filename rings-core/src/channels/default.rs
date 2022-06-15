use async_channel as ac;
use async_channel::Receiver;
use async_channel::Sender;
use async_trait::async_trait;

use crate::err::Error;
use crate::err::Result;
use crate::types::channel::Channel;

pub struct AcChannel<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

#[async_trait]
impl<T: Send> Channel<T> for AcChannel<T> {
    type Sender = Sender<T>;
    type Receiver = Receiver<T>;

    fn new() -> Self {
        let (tx, rx) = ac::unbounded();
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
            Err(_) => Err(Error::ChannelSendMessageFailed),
        }
    }

    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>> {
        match receiver.recv().await {
            Ok(v) => Ok(Some(v)),
            Err(_) => Err(Error::ChannelRecvMessageFailed),
        }
    }
}
