use crate::types::channel::Channel;
use crate::types::channel::Event;
/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs
use anyhow::Result;
use async_trait::async_trait;
//use crossbeam_channel as cbc;
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::SinkExt;
use std::sync::Arc;

#[derive(Debug)]
pub struct CbChannel {
    sender: Arc<Mutex<mpsc::Sender<Event>>>,
    receiver: Arc<Mutex<mpsc::Receiver<Event>>>,
}

#[async_trait(?Send)]
impl Channel for CbChannel {
    type Sender = Arc<Mutex<mpsc::Sender<Event>>>;
    type Receiver = Arc<Mutex<mpsc::Receiver<Event>>>;

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

    async fn send(&self, e: Event) -> Result<()> {
        let mut sender = self.sender.lock().await;
        Ok(sender.send(e).await?)
    }

    async fn recv(&self) -> Result<Event> {
        let mut receiver = self.receiver.lock().await;
        match receiver.try_next() {
            Ok(Some(e)) => Ok(e),
            Ok(None) => Err(anyhow::anyhow!("received empty msg")),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    }
}
