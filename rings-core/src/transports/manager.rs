use std::sync::Arc;

use async_trait::async_trait;
use web3::types::Address;

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

    fn get_transports(&self) -> Vec<(Address, Self::Transport)>;
    fn get_addresses(&self) -> Vec<Address>;
    fn get_transport(&self, address: &Address) -> Option<Self::Transport>;
    fn remove_transport(&self, address: &Address) -> Option<(Address, Self::Transport)>;
    fn get_transport_numbers(&self) -> usize;
    async fn get_and_check_transport(&self, address: &Address) -> Option<Self::Transport>;
    async fn new_transport(&self) -> Result<Self::Transport>;
    async fn register(&self, address: &Address, trans: Self::Transport) -> Result<()>;
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
    async fn register(&self, address: &Address, trans: Self::Transport) -> Result<()> {
        if trans.is_disconnected().await {
            return Err(Error::InvalidTransport);
        }

        log::info!("register transport {:?}", trans.id.clone());
        #[cfg(test)]
        {
            println!("register transport {:?}", trans.id.clone());
        }
        let id = trans.id;
        if let Some(t) = self.transports.get(address) {
            if t.is_connected().await && !trans.is_connected().await {
                return Err(Error::InvalidTransport);
            }
            if t.id != id {
                self.transports.set(address, trans);
                if let Err(e) = t.close().await {
                    log::error!("failed to close previous while registering {:?}", e);
                    return Err(Error::SwarmToClosePrevTransport(format!("{:?}", e)));
                }
                log::debug!("replace and closed previous connection! {:?}", t.id);
            }
        } else {
            self.transports.set(address, trans);
        }
        Ok(())
    }

    async fn get_and_check_transport(&self, address: &Address) -> Option<Self::Transport> {
        match self.get_transport(address) {
            Some(t) => {
                if t.is_disconnected().await {
                    log::debug!(
                        "[get_and_check_transport] transport {:?} is not connected will be drop",
                        t.id
                    );
                    if t.close().await.is_err() {
                        log::error!("Failed on close transport");
                    };
                    None
                } else {
                    Some(t)
                }
            }
            None => None,
        }
    }

    fn get_transport(&self, address: &Address) -> Option<Self::Transport> {
        self.transports.get(address)
    }

    fn remove_transport(&self, address: &Address) -> Option<(Address, Self::Transport)> {
        self.transports.remove(address)
    }

    fn get_transport_numbers(&self) -> usize {
        self.transports.len()
    }

    fn get_addresses(&self) -> Vec<Address> {
        self.transports.keys()
    }

    fn get_transports(&self) -> Vec<(Address, Self::Transport)> {
        self.transports.items()
    }
}
