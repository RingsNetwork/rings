use crate::types::channel::Channel;
/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs
use async_trait::async_trait;
//use crossbeam_channel as cbc;
use anyhow::Result;
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
impl<T> Channel for CbChannel<T> {
    type Sender = Sender<T>;
    type Receiver = Receiver<T>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer);
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
}

pub async fn recv<T>(receiver: &Mutex<mpsc::Receiver<T>>) -> Result<T> {
    let mut receiver = receiver.lock().await;
    match receiver.try_next() {
        Ok(Some(e)) => Ok(e),
        Ok(None) => Err(anyhow::anyhow!("received empty msg")),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}
