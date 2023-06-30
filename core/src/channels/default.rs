//! Default channel handle for `node` feature.
use async_channel as ac;
use async_channel::Receiver;
use async_channel::Sender;
use async_trait::async_trait;

use crate::error::Error;
use crate::error::Result;
use crate::types::channel::Channel;

/// Channel combine with async_channel::Sender and async_channel::Receiver.
#[derive(Debug)]
pub struct AcChannel<T> {
    /// async channel send `Message` through sender.
    sender: Sender<T>,
    /// async channel rece `Message` through receiver.
    receiver: Receiver<T>,
}

#[async_trait]
impl<T: Send> Channel<T> for AcChannel<T>
where T: std::fmt::Debug
{
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
        tracing::debug!("channel sending message: {:?}", msg);
        match sender.send(msg).await {
            Ok(_) => {
                tracing::debug!("channel send message success");
                Ok(())
            }
            Err(_) => Err(Error::ChannelSendMessageFailed),
        }
    }

    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>> {
        match receiver.recv().await {
            Ok(v) => {
                tracing::debug!("channel received message: {:?}", v);
                Ok(Some(v))
            }
            Err(_) => Err(Error::ChannelRecvMessageFailed),
        }
    }
}
