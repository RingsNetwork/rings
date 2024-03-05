#![warn(missing_docs)]
use async_trait::async_trait;

use super::storage::handle_storage_store_act;
use crate::dht::ChordStorage;
use crate::dht::PeerRing;
use crate::error::Result;
use crate::prelude::vnode::VNodeOperation;
use crate::swarm::Swarm;

/// SubringInterface should imply necessary operator for DHT Subring
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SubringInterface<const REDUNDANT: u16> {
    /// join a subring
    async fn subring_join(&self, name: &str) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl<const REDUNDANT: u16> SubringInterface<REDUNDANT> for Swarm {
    /// add did into current chord subring.
    /// send direct message with `JoinSubring` type, which will handled by `next` node.
    async fn subring_join(&self, name: &str) -> Result<()> {
        let op = VNodeOperation::JoinSubring(name.to_string(), self.dht.did);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::vnode_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }
}
