#![warn(missing_docs)]
use async_trait::async_trait;

use crate::dht::ChordStorage;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Error;
use crate::err::Result;
use crate::message::types::Message;
use crate::message::PayloadSender;
use crate::prelude::vnode::VNodeOperation;
use crate::swarm::Swarm;

/// SubringInterface should imply necessary operator for DHT Subring
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SubringInterface {
    /// join a subring
    async fn subring_join(&self, name: &str) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl SubringInterface for Swarm {
    /// add did into current chord subring.
    /// send direct message with `JoinSubring` type, which will handled by `next` node.
    async fn subring_join(&self, name: &str) -> Result<()> {
        let op = VNodeOperation::JoinSubring(name.to_string(), self.dht.did);
        match self.dht.vnode_operate(op).await? {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindVNodeForOperate(op)) => {
                self.send_direct_message(Message::OperateVNode(op), target)
                    .await?;
                Ok(())
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }
}
