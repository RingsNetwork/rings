use crate::types::channel::Channel;
use crate::types::channel::Event;
/// ref: https://github.com/Ciantic/rust-shared-wasm-experiments/blob/master/src/lib.rs
use anyhow::Result;
use async_trait::async_trait;
use crossbeam_channel as cbc;

#[derive(Clone, Debug)]
pub struct CbChannel {
    sender: cbc::Sender<Event>,
    receiver: cbc::Receiver<Event>,
}

#[async_trait(?Send)]
impl Channel for CbChannel {
    type Sender = cbc::Sender<Event>;
    type Receiver = cbc::Receiver<Event>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = cbc::bounded(buffer);
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

    async fn send(&self, e: Event) -> Result<()> {
        Ok(self.sender.send(e)?)
    }

    async fn recv(&self) -> Result<Event> {
        self.receiver().recv().map_err(|e| anyhow::anyhow!(e))
    }
}
