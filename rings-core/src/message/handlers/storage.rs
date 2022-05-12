use crate::dht::{ChordStorage, Did, PeerRingAction, PeerRingRemoteAction};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::protocol::MessageSessionRelayProtocol;
use crate::message::types::{FoundVNode, Message, SearchVNode, StoreVNode, SyncVNodeWithSuccessor};
use crate::message::MessageHandler;

use async_trait::async_trait;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordStorage {
    async fn search_vnode(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: SearchVNode,
    ) -> Result<()>;

    async fn found_vnode(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: FoundVNode,
    ) -> Result<()>;

    async fn store_vnode(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: StoreVNode,
    ) -> Result<()>;

    async fn sync_with_successor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: SyncVNodeWithSuccessor,
    ) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordStorage for MessageHandler {
    async fn search_vnode(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: SearchVNode,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        match dht.lookup(msg.target_id) {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::SomeVNode(v) => {
                    self.send_message(
                        &prev.into(),
                        Some(relay.from_path),
                        Some(relay.to_path),
                        MessageRelayMethod::REPORT,
                        Message::FoundVNode(FoundVNode {
                            target_id: msg.sender_id,
                            data: vec![v],
                        }),
                    )
                    .await
                }
                PeerRingAction::RemoteAction(next, _) => {
                    self.send_message(
                        &next.into(),
                        Some(relay.to_path),
                        Some(relay.from_path),
                        MessageRelayMethod::SEND,
                        Message::SearchVNode(SearchVNode {
                            sender_id: msg.sender_id,
                            target_id: msg.target_id,
                        }),
                    )
                    .await
                }
                act => Err(Error::PeerRingUnexpectedAction(act)),
            },
            Err(e) => Err(e),
        }
    }

    async fn found_vnode(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: FoundVNode,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        if !relay.to_path.is_empty() {
            self.send_message(
                &prev.into(),
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                Message::FoundVNode(msg.clone()),
            )
            .await
        } else {
            // found vnode and TODO
            Ok(())
        }
    }

    async fn store_vnode(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: StoreVNode,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        let virtual_peer = msg.data.clone();
        for p in virtual_peer {
            match dht.store(p) {
                Ok(action) => match action {
                    PeerRingAction::None => Ok(()),
                    PeerRingAction::RemoteAction(next, _) => {
                        self.send_message(
                            &next.into(),
                            Some(relay.to_path.clone()),
                            Some(relay.from_path.clone()),
                            MessageRelayMethod::SEND,
                            Message::StoreVNode(msg.clone()),
                        )
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
        prev: Did,
        msg: SyncVNodeWithSuccessor,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        for data in msg.data {
            // only simply store here
            match dht.store(data) {
                Ok(PeerRingAction::None) => Ok(()),
                Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindAndStore(peer),
                )) => {
                    self.send_message(
                        &next.into(),
                        Some(relay.to_path.clone()),
                        Some(relay.from_path.clone()),
                        MessageRelayMethod::SEND,
                        Message::StoreVNode(StoreVNode {
                            sender_id: msg.sender_id,
                            data: vec![peer],
                        }),
                    )
                    .await
                }
                Ok(_) => unreachable!(),
                Err(e) => Err(e),
            }?;
        }
        Ok(())
    }
}
