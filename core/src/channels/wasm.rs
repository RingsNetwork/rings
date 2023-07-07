//! Async channel handle for `wasm` features.
use std::sync::Arc;

/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs
use async_trait::async_trait;
use futures::channel::mpsc;
use futures::lock::Mutex;

use crate::error::Error;
use crate::error::Result;
use crate::types::channel::Channel;

type Sender<T> = Arc<mpsc::UnboundedSender<T>>;
type Receiver<T> = Arc<Mutex<mpsc::UnboundedReceiver<T>>>;

/// Channel combine with mpsc::UnboundedSender and mpsc::UnboundedReceiver.
#[derive(Debug)]
pub struct CbChannel<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

#[async_trait(?Send)]
impl<T: Send> Channel<T> for CbChannel<T> {
    type Sender = Sender<T>;
    type Receiver = Receiver<T>;

    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded();
        Self {
            sender: Arc::new(tx),
            receiver: Arc::new(Mutex::new(rx)),
        }
    }

    fn sender(&self) -> Self::Sender {
        self.sender.clone()
    }

    fn receiver(&self) -> Self::Receiver {
        self.receiver.clone()
    }

    /// Sends a message along this channel.
    /// This is an unbounded sender, so this function differs from Sink::send
    /// by ensuring the return type reflects that the channel is always ready to receive messages.
    /// Err(TrySendError) can caused by `is_full` or `is_disconnected`
    /// ref: https://docs.rs/futures/latest/futures/channel/mpsc/struct.UnboundedSender.html
    async fn send(sender: &Self::Sender, msg: T) -> Result<()> {
        match sender.unbounded_send(msg) {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::ChannelSendMessageFailed),
        }
    }

    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>> {
        let mut receiver = receiver.lock().await;
        match receiver.try_next() {
	    // when there are no messages available, but channel is not yet closed
            Err(_) => Ok(None),
	    // when message is fetched
            Ok(Some(x)) => Ok(Some(x)),
            // when channel is closed and no messages left in the queue
            Ok(None) => Ok(None),
        }
    }
}
