#![warn(missing_docs)]
use std::str::FromStr;

use async_trait::async_trait;

use super::storage::TChordStorage;
use crate::dht::subring::SubRing;
use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction as RemoteAction;
use crate::dht::SubRingManager;
use crate::ecc::HashStr;
use crate::err::Error;
use crate::err::Result;
use crate::message::types::JoinSubRing;
use crate::message::types::Message;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

/// SubRingOperator should imply necessary operator for DHT SubRing
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SubRingOperator {
    /// Create subring
    /// 1. Created a subring and stored in Handler.subrings
    /// 2. Send StoreVNode message to it's successor
    async fn create(&self, name: &str) -> Result<()>;
    /// join a subring
    async fn join(&self, name: &str) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl SubRingOperator for MessageHandler {
    async fn create(&self, name: &str) -> Result<()> {
        let dht = self.dht.lock().await;
        let subring: SubRing = SubRing::new(name, &dht.id)?;
        let vnode: VirtualNode = subring.clone().try_into()?;
        dht.store_subring(&subring.clone())?;
        self.store(vnode).await
    }

    async fn join(&self, name: &str) -> Result<()> {
        let dht = self.dht.lock().await;
        let address: HashStr = name.to_owned().into();
        let did = Did::from_str(&address.inner())?;
        match dht.join_subring(&dht.id, &did) {
            Ok(PeerRingAction::RemoteAction(next, RemoteAction::FindAndJoinSubRing(rid))) => {
                self.send_direct_message(Message::JoinSubRing(JoinSubRing { did: rid }), next)
                    .await
            }
            Ok(PeerRingAction::None) => Ok(()),
            Ok(act) => Err(Error::PeerRingUnexpectedAction(act)),
            Err(e) => Err(e),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<JoinSubRing> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &JoinSubRing) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();
        let origin = relay.origin();
        match dht.join_subring(&origin, &msg.did) {
            Ok(PeerRingAction::RemoteAction(next, RemoteAction::FindAndJoinSubRing(_))) => {
                relay.relay(dht.id, Some(next))?;
                relay.reset_destination(next)?;
                self.transpond_payload(ctx, relay).await
            }
            Ok(PeerRingAction::None) => Ok(()),
            Ok(act) => Err(Error::PeerRingUnexpectedAction(act)),
            Err(e) => Err(e),
        }
    }
}
