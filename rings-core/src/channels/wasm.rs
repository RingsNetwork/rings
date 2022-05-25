use crate::err::{Error, Result};
use crate::types::channel::Channel;
/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs
use async_trait::async_trait;
use futures::channel::mpsc;
use futures::lock::Mutex;
use std::sync::Arc;

type Sender<T> = Arc<Mutex<mpsc::Sender<T>>>;
type Receiver<T> = Arc<Mutex<mpsc::Receiver<T>>>;

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
        let (tx, rx) = mpsc::channel(8);
        Self {
            sender: Arc::new(Mutex::new(tx)),
            receiver: Arc::new(Mutex::new(rx)),
        }
    }

    fn sender(&self) -> Self::Sender {
        self.sender.clone()
    }

    fn receiver(&self) -> Self::Receiver {
        self.receiver.clone()
    }

    async fn send(sender: &Self::Sender, msg: T) -> Result<()> {
        let mut sender = sender.lock().await;
        assert!(!sender.is_closed());
        match sender.try_send(msg) {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::ChannelSendMessageFailed),
        }
    }

    async fn recv(receiver: &Self::Receiver) -> Result<Option<T>> {
        let mut receiver = receiver.lock().await;
        match receiver.try_next() {
            Err(_) => Err(Error::ChannelRecvMessageFailed),
            Ok(Some(x)) => Ok(Some(x)),
            // when channel is closed and no messages left in the queue
            Ok(None) => Ok(None),
        }
    }
}
