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

use anyhow::anyhow;
use anyhow::Result;
use dashmap::DashMap;
use std::sync::Arc;

pub enum State {
    Anonymous,
    Known,
}

#[derive(Clone)]
pub struct Swarm {
    pub pending: Option<Arc<Transport>>,
    pub anonymous: DashMap<String, Arc<Transport>>,
    pub table: DashMap<String, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: String) -> Self {
        Self {
            pending: None,
            anonymous: DashMap::new(),
            table: DashMap::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun,
        }
    }

    pub async fn new_transport(&mut self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(Arc::clone(&self.signaler));
        ice_transport.start(self.stun_server.clone()).await?;
        // should always has offer here #WhyUnwarp
        let trans = Arc::new(ice_transport);
        self.pending = Some(Arc::clone(&trans));
        Ok(Arc::clone(&trans))
    }

    pub async fn get_pending(&mut self) -> Option<Arc<Transport>> {
        match &self.pending {
            Some(trans) => Some(Arc::clone(trans)),
            None => self.new_transport().await.ok(),
        }
    }

    pub async fn drop_pending(&mut self) {
        self.pending = None;
    }

    pub async fn upgrade_pending(&mut self) -> Result<()> {
        let trans = self.pending.take();
        match &trans {
            Some(t) => {
                let sdp = t.get_offer_str().await.unwrap();
                self.register(sdp, Arc::clone(t), State::Anonymous);
                Ok(())
            }
            None => Err(anyhow!("pending transport not exiest")),
        }?;
        self.pending = None;
        Ok(())
    }

    pub fn register(&mut self, sdp_or_addr: String, trans: Arc<Transport>, trans_type: State) {
        match trans_type {
            State::Anonymous => {
                self.anonymous.insert(sdp_or_addr, Arc::clone(&trans));
            }
            State::Known => {
                self.table.insert(sdp_or_addr, Arc::clone(&trans));
            }
        }
    }

    pub fn get_transport(&self, sdp_or_addr: String, trans_type: State) -> Option<Arc<Transport>> {
        match trans_type {
            State::Anonymous => self.anonymous.get(&sdp_or_addr).map(|t| Arc::clone(&t)),
            State::Known => self.table.get(&sdp_or_addr).map(|t| Arc::clone(&t)),
        }
    }

    pub fn upgrade_anonymous(&mut self, sdp: String, addr: String) -> Result<()> {
        match self.get_transport(sdp.to_owned(), State::Anonymous) {
            Some(trans) => {
                self.register(addr, Arc::clone(&trans), State::Known);
                self.anonymous.remove(&sdp);
                Ok(())
            }
            None => Err(anyhow!("Cannot get asked Anonymous Transport")),
        }
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
    }
}
