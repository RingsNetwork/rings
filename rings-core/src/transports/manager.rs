use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;
use crate::err::Error;
use crate::err::Result;
use crate::swarm::Swarm;
use crate::transports::Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::ice_transport::IceTransportInterface;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TransportManager {
    type Transport;

    fn get_transports(&self) -> Vec<(Did, Self::Transport)>;
    fn get_dids(&self) -> Vec<Did>;
    fn get_transport(&self, did: Did) -> Option<Self::Transport>;
    fn remove_transport(&self, did: Did) -> Option<(Did, Self::Transport)>;
    fn get_transport_numbers(&self) -> usize;
    async fn get_and_check_transport(&self, did: Did) -> Option<Self::Transport>;
    async fn new_transport(&self) -> Result<Self::Transport>;
    async fn register(&self, did: Did, trans: Self::Transport) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportManager for Swarm {
    type Transport = Arc<Transport>;

    async fn new_transport(&self) -> Result<Self::Transport> {
        let event_sender = self.transport_event_channel.sender();
        let mut ice_transport = Transport::new(event_sender);
        ice_transport
            .start(self.ice_servers.clone(), self.external_address.clone())
            .await?
            .apply_callback()
            .await?;

        Ok(Arc::new(ice_transport))
    }

    // register to swarm transports
    // should not wait connection statues here
    // a connection `Promise` may cause deadlock of both end
    async fn register(&self, did: Did, trans: Self::Transport) -> Result<()> {
        if trans.is_disconnected().await {
            return Err(Error::InvalidTransport);
        }

        tracing::info!("register transport {:?}", trans.id.clone());
        #[cfg(test)]
        {
            println!("register transport {:?}", trans.id.clone());
        }
        let id = trans.id;
        if let Some(t) = self.transports.get(&did) {
            if t.is_connected().await && !trans.is_connected().await {
                return Err(Error::InvalidTransport);
            }
            if t.id != id {
                self.transports.set(&did, trans);
                if let Err(e) = t.close().await {
                    tracing::error!("failed to close previous while registering {:?}", e);
                    return Err(Error::SwarmToClosePrevTransport(format!("{:?}", e)));
                }
                tracing::debug!("replace and closed previous connection! {:?}", t.id);
            }
        } else {
            self.transports.set(&did, trans);
        }
        Ok(())
    }

    async fn get_and_check_transport(&self, did: Did) -> Option<Self::Transport> {
        match self.get_transport(did) {
            Some(t) => {
                if t.is_disconnected().await {
                    tracing::debug!(
                        "[get_and_check_transport] transport {:?} is not connected will be drop",
                        t.id
                    );
                    if t.close().await.is_err() {
                        tracing::error!("Failed on close transport");
                    };
                    None
                } else {
                    Some(t)
                }
            }
            None => None,
        }
    }

    fn get_transport(&self, did: Did) -> Option<Self::Transport> {
        self.transports.get(&did)
    }

    fn remove_transport(&self, did: Did) -> Option<(Did, Self::Transport)> {
        self.transports.remove(&did)
    }

    fn get_transport_numbers(&self) -> usize {
        self.transports.len()
    }

    fn get_dids(&self) -> Vec<Did> {
        self.transports.keys()
    }

    fn get_transports(&self) -> Vec<(Did, Self::Transport)> {
        self.transports.items()
    }
}
