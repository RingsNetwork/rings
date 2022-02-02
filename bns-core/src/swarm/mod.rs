/// Swarm is transport management
///
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;

#[cfg(not(feature = "wasm"))]
use crate::channels::default::TkChannel as Channel;
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
use std::sync::Mutex;

pub enum State {
    Anonymous,
    Known,
}

#[derive(Clone)]
pub struct Swarm {
    pub pending: Option<Arc<Transport>>,
    pub anonymous: DashMap<String, Arc<Transport>>,
    pub table: DashMap<String, Arc<Transport>>,
    pub signaler: Arc<Mutex<Channel>>,
    pub stun_server: String
}

impl Swarm {
    pub fn new(ch: Channel, stun_server: String) -> Self {
        Self {
            pending: None,
            anonymous: DashMap::new(),
            table: DashMap::new(),
            signaler: Arc::new(Mutex::new(ch)),
            stun_server: stun_server.to_owned()
        }
    }

    pub async fn new_transport(&mut self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(Arc::clone(&self.signaler));
        ice_transport.start(self.stun_server.to_owned()).await?;
        ice_transport.setup_callback().await?;
        // should always has offer here #WhyUnwarp
        let trans = Arc::new(ice_transport);
        Ok(Arc::clone(&trans))
    }

    pub async fn get_pending(&mut self) -> Option<Arc<Transport>> {
        match &self.pending {
            Some(trans) => {
                log::info!("Pending transport exists");
                Some(Arc::clone(trans))
            },
            None => {
                log::info!("Pending transport not exists, creating new");
                match self.new_transport().await {
                    Ok(t) => {
                        self.pending = Some(Arc::clone(&t));
                        log::info!("assigning new transport");
                        Some(t)
                    },
                    Err(e) => {
                        log::error!("{}", e);
                        None
                    }
                }
            }
        }
    }

    pub fn upgrade_pending(&mut self) -> Result<()> {
        let trans = self.pending.take();
        match &trans {
            Some(t) => {
                let sdp = t.get_offer_str().unwrap();
                self.register(sdp, Arc::clone(t), State::Anonymous);
                Ok(())
            }
            None => Err(anyhow!("pending transport not exiest")),
        }?;
        log::info!("set pending transport to None");
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

    pub fn signaler(&self) -> Arc<Mutex<Channel>> {
        Arc::clone(&self.signaler)
    }
}
