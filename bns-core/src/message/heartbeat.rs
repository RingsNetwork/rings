use crate::message::{Message, MessageRelay, MessageRelayMethod};
use crate::swarm::{Swarm, TransportManager};
#[cfg(not(feature = "wasm"))]
use crate::transports::default::DefaultTransport as Transport;
#[cfg(feature = "wasm")]
use crate::transports::wasm::WasmTransport as Transport;
use crate::types::ice_transport::IceTransport;

use anyhow::Result;
use web3::types::Address;

use std::sync::Arc;
#[cfg(feature = "wasm")]
use std::{
    thread::{JoinHandle, spawn},
    sync::mpsc::{channel, Sender, Receiver},
    time
};
#[cfg(not(feature = "wasm"))]
use tokio::{
    sync::mpsc::{channel, Sender, Receiver},
    task::{JoinHandle, spawn},
    time
};

pub struct Heartbeat {
    swarm: Arc<Swarm>,
}


impl Heartbeat {
    pub fn new(swarm: Arc<Swarm>) -> Self {
        Self {
            swarm
        }
    }

    pub async fn start(&self, items: Vec<(Address, Arc<Transport>)>) -> Result<()> {
        let key = self.swarm.key.to_owned();
        let (tx, mut rx) = channel::<(Address, bool)>(items.len());
        let tasks: Vec<JoinHandle<()>> = items
            .into_iter()
            .map(|(address, transport)| {
                let address = address;
                let dispatcher = tx.clone();
                let transport = Arc::clone(&transport);
                async move {
                    let message_relay = MessageRelay::new(
                        Message::Ping,
                        &key,
                        None,
                        None,
                        None,
                        MessageRelayMethod::SEND,
                    )
                    .unwrap();
                    match transport.send_message(message_relay).await {
                        Ok(_) => {
                            log::debug!("connect success");
                            dispatcher.send((address, true)).await.unwrap();
                        }
                        Err(_) => {
                            dispatcher.send((address, false)).await.unwrap();
                        }
                    }
                }
            })
            .into_iter()
            .map(spawn)
            .collect();
        futures::future::join_all(tasks).await;
        while let Some((address, signal)) = rx.recv().await {
            if !signal {
                self.swarm.remove_transport(&address);
            }
        }
        Ok(())
    }

    pub async fn run(&self){
        loop {
            if self.swarm.get_transport_numbers() == 0 {
                time::sleep(time::Duration::from_secs(20)).await;
            } else {
                let items = self.swarm.get_transports();
                match self.start(items).await {
                    Ok(()) => time::sleep(time::Duration::from_secs(10)).await,
                    Err(e) => panic!("Heartbeat went wrong, {:?}", e)
                }
            }
        }
    }
}
