use crate::types::channel::Channel;
use crate::types::channel::Events;
/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs
use anyhow::Result;
use async_trait::async_trait;
use crossbeam_channel as cbc;
use log::info;
use std::sync::Arc;

pub struct CbChannel {
    sender: Arc<cbc::Sender<Events>>,
    receiver: Arc<cbc::Receiver<Events>>,
}

#[async_trait(?Send)]
impl Channel for CbChannel {
    type Sender = Arc<cbc::Sender<Events>>;
    type Receiver = Arc<cbc::Receiver<Events>>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = cbc::bounded(buffer);
        Self {
            sender: Arc::new(tx),
            receiver: Arc::new(rx),
        }
    }

    fn sender(&self) -> Self::Sender {
        Arc::clone(&self.sender)
    }

    fn receiver(&self) -> Self::Receiver {
        Arc::clone(&self.receiver)
    }

    async fn send(&self, e: Events) -> Result<()> {
        Ok(self.sender.send(e)?)
    }

    async fn recv(&self) {
        match self.receiver().recv() {
            Ok(e) => {
                _ = self.handler(e).await;
            }
            Err(e) => {
                log::error!("failed on recv: {:?}", e);
            }
        }
    }
    async fn handler(&self, e: Events) {
        match e {
            Events::Null => (),
            Events::ConnectFailed => {
                info!("ConnectFailed");
            }
            _ => (),
        }
    }
}
