use super::storage::TChordStorage;
use crate::dht::vnode::VirtualNode;
use crate::err::Result;
use crate::message::MessageHandler;

impl MessageHandler {
    /// Create subring
    /// 1. Created a subring and stored in Handler.subrings
    /// 2. Send StoreVNode message to it's successor
    pub async fn create_or_join_subring(&self, name: &String) -> Result<()> {
        let subring: VirtualNode = self.subrings.create_subring(&name)?.try_into()?;
        self.store(subring).await
    }
}
