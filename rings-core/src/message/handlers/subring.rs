#![warn(missing_docs)]

use super::storage::TChordStorage;
use crate::dht::subring::SubRing;
use crate::dht::subring::TSubRingManager;
use crate::dht::vnode::VirtualNode;
use crate::err::Result;
use crate::message::MessageHandler;

impl MessageHandler {
    /// Create subring
    /// 1. Created a subring and stored in Handler.subrings
    /// 2. Send StoreVNode message to it's successor
    pub async fn create(&self, name: &str) -> Result<()> {
        let dht = self.dht.lock().await;
        let subring: SubRing = SubRing::new(name, &dht.id)?;
        let vnode: VirtualNode = subring.clone().try_into()?;
        dht.store_subring(&subring.clone())?;
        self.store(vnode).await
    }
}
