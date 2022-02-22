#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
use crate::dht::chord::Chord;
/// Swarm is transport management
///
use crate::storage::{MemStorage, Storage};
#[cfg(not(feature = "wasm"))]
use crate::transports::default::DefaultTransport as Transport;
#[cfg(feature = "wasm")]
use crate::transports::wasm::WasmTransport as Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use anyhow::Result;
use std::sync::Arc;
use web3::types::Address;

pub struct Swarm {
    pub table: MemStorage<Address, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
    pub dht: Chord,
    pub address: Address,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: String, addr: Address) -> Self {
        Self {
            table: MemStorage::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun,
            dht: Chord::new(addr.into()),
            address: addr.clone(),
        }
    }

    pub async fn new_transport(&self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(Arc::clone(&self.signaler));
        ice_transport.start(self.stun_server.clone()).await?;
        let trans = Arc::new(ice_transport);
        Ok(Arc::clone(&trans))
    }

    pub fn register(&self, addr: Address, trans: Arc<Transport>) {
        self.table.set(addr, trans);
    }

    pub fn get_transport(&self, addr: Address) -> Option<Arc<Transport>> {
        self.table.get(addr)
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
    }

    pub async fn event_handler(&self) {
        loop {
            match self.signaler.recv().await {
                Ok(ev) => match ev {
                    Events::ReceiveMsg(m) => {
                        let m = String::from_utf8(m).unwrap();
                        log::debug!("Receive Msg {}", m);
                    }
                    x => {
                        log::debug!("Receive {:?}", x)
                    }
                },
                Err(e) => {
                    log::error!("failed on handle event {:?}", e)
                }
            }
        }
    }
}
