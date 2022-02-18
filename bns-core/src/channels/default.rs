use crate::types::channel::Channel;
use crate::types::channel::Events;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;

pub struct TkChannel {
    sender: Arc<mpsc::Sender<Events>>,
    receiver: Arc<Mutex<mpsc::Receiver<Events>>>,
}

#[async_trait(?Send)]
impl Channel for TkChannel {
    type Sender = Arc<mpsc::Sender<Events>>;
    type Receiver = Arc<Mutex<mpsc::Receiver<Events>>>;

    fn new(buffer: usize) -> Self {
        let (tx, rx) = mpsc::channel::<Events>(buffer);
        Self {
            sender: Arc::new(tx),
            receiver: Arc::new(Mutex::new(rx)),
        }
    }

    fn sender(&self) -> Self::Sender {
        Arc::clone(&self.sender)
    }

    fn receiver(&self) -> Self::Receiver {
        Arc::clone(&self.receiver)
    }

    async fn send(&self, e: Events) -> Result<()> {
        return Ok(self.sender.send(e).await?);
    }

    async fn recv(&self) {
        let receiver = self.receiver.lock();
        match receiver {
            Ok(mut recv) => {
                match recv.recv().await {
                    Some(e) => {
                        _ = self.handler(e).await;
                    },
                    None => ()
                }
            },
            Err(e) => {
                log::error!("cannot lock channel mutex {:?}", e);
            }
        }
    }
    async fn handler(&self, e: Events) {
        match e {
            Events::Null => (),
            Events::ConnectFailed => {
                println!("ConnectFailed");
            }
            _ => (),
        }
    }
}
