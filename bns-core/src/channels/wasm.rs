/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use crate::types::channel::Events;
use crate::types::channel::Channel;
use crossbeam_channel as cbc;
use log::info;


pub struct CbChannel {
    sender: Arc<cbc::Sender<Events>>,
    receiver: cbc::Receiver<Events>
}


#[async_trait(?Send)]
impl Channel for CbChannel {
    type Sender = Arc<cbc::Sender<Events>>;
    type Receiver = cbc::Receiver<Events>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = cbc::bounded(buffer);
        return Self {
            sender: Arc::new(tx),
            receiver: rx
        }
    }

    fn sender(&self) -> Self::Sender {
        return Arc::clone(&self.sender);
    }

    async fn send(&self, e: Events) -> Result<()> {
        return Ok(self.sender.send(e)?);
    }

    async fn recv(&mut self) -> () {
        while let Ok(e) = self.receiver.recv() {
            let _ = self.handler(e);
        }

    }

    async fn handler(&self, e: Events) {
        match e {
            Events::Null => (),
            Events::ConnectFailed => {
                info!("ConnectFailed");
            }
        }
    }

}
