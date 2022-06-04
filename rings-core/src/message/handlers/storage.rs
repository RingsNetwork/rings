use crate::dht::{ChordStorage, PeerRingAction, PeerRingRemoteAction};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::protocol::MessageSessionRelayProtocol;
use crate::message::types::{FoundVNode, Message, SearchVNode, StoreVNode, SyncVNodeWithSuccessor};
use crate::message::{MessageHandler, OriginVerificationGen};

use async_trait::async_trait;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordStorage {
    async fn search_vnode(&self, relay: MessageRelay<Message>, msg: SearchVNode) -> Result<()>;
    async fn found_vnode(&self, relay: MessageRelay<Message>, msg: FoundVNode) -> Result<()>;
    async fn store_vnode(&self, relay: MessageRelay<Message>, msg: StoreVNode) -> Result<()>;
    async fn sync_with_successor(
        &self,
        relay: MessageRelay<Message>,
        msg: SyncVNodeWithSuccessor,
    ) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordStorage for MessageHandler {
    async fn search_vnode(&self, relay: MessageRelay<Message>, msg: SearchVNode) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        match dht.lookup(msg.target_id) {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::SomeVNode(v) => {
                    relay.relay(dht.id, None)?;
                    self.send_relay_message(relay.report(
                        Message::FoundVNode(FoundVNode {
                            target_id: msg.sender_id,
                            data: vec![v],
                        }),
                        &self.swarm.session_manager,
                    )?)
                    .await
                }
                PeerRingAction::RemoteAction(next, _) => {
                    relay.relay(dht.id, Some(next))?;
                    self.send_relay_message(relay.rewrap(
                        relay.data.clone(),
                        &self.swarm.session_manager,
                        OriginVerificationGen::Stick(relay.origin_verification.clone()),
                    )?)
                    .await
                }
                act => Err(Error::PeerRingUnexpectedAction(act)),
            },
            Err(e) => Err(e),
        }
    }

    async fn found_vnode(&self, relay: MessageRelay<Message>, _msg: FoundVNode) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.relay(dht.id, None)?;
        if relay.next_hop.is_some() {
            self.send_relay_message(relay.rewrap(
                relay.data.clone(),
                &self.swarm.session_manager,
                OriginVerificationGen::Stick(relay.origin_verification.clone()),
            )?)
            .await
        } else {
            // found vnode and TODO
            Ok(())
        }
    }

    async fn store_vnode(&self, relay: MessageRelay<Message>, msg: StoreVNode) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        let virtual_peer = msg.data.clone();
        for p in virtual_peer {
            match dht.store(p) {
                Ok(action) => match action {
                    PeerRingAction::None => Ok(()),
                    PeerRingAction::RemoteAction(next, _) => {
                        relay.reset_destination(next)?;
                        relay.relay(dht.id, Some(next))?;
                        self.send_relay_message(relay.rewrap(
                            relay.data.clone(),
                            &self.swarm.session_manager,
                            OriginVerificationGen::Stick(relay.origin_verification.clone()),
                        )?)
                        .await
                    }
                    act => Err(Error::PeerRingUnexpectedAction(act)),
                },
                Err(e) => Err(e),
            }?;
        }
        Ok(())
    }

    // received remote sync vnode request
    async fn sync_with_successor(
        &self,
        relay: MessageRelay<Message>,
        msg: SyncVNodeWithSuccessor,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        for data in msg.data {
            // only simply store here
            match dht.store(data) {
                Ok(PeerRingAction::None) => Ok(()),
                Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindAndStore(peer),
                )) => {
                    relay.relay(dht.id, Some(next))?;
                    self.send_relay_message(MessageRelay::new(
                        Message::StoreVNode(StoreVNode { data: vec![peer] }),
                        &self.swarm.session_manager,
                        OriginVerificationGen::Origin,
                        MessageRelayMethod::SEND,
                        None,
                        None,
                        Some(next),
                        next,
                    )?)
                    .await
                }
                Ok(_) => unreachable!(),
                Err(e) => Err(e),
            }?;
        }
        Ok(())
    }
}
