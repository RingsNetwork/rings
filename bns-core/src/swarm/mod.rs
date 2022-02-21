/// Swarm is transport management
///
use crate::types::ice_transport::IceTransport;

#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(not(feature = "wasm"))]
use crate::transports::default::DefaultTransport as Transport;

#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
#[cfg(feature = "wasm")]
use crate::transports::wasm::WasmTransport as Transport;

use web3::types::Address;
use anyhow::Result;
use dashmap::DashMap;
use std::sync::Arc;

pub enum State {
    Anonymous,
    Known,
}

pub struct Swarm {
    pub table: DashMap<Address, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: String) -> Self {
        Self {
            table: DashMap::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun,
        }
    }

    pub async fn new_transport(&self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(Arc::clone(&self.signaler));
        ice_transport.start(self.stun_server.clone()).await?;
        let trans = Arc::new(ice_transport);
        Ok(Arc::clone(&trans))
    }

    pub fn register(&self, addr: Address, trans: Arc<Transport>) {
        self.table.insert(addr, Arc::clone(&trans));
    }

    pub fn get_transport(&self, addr: Address) -> Option<Arc<Transport>> {
        self.table.get(&addr).map(|t| Arc::clone(&t))
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
    }
}
