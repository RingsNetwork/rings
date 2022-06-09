use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::dht::{ChordStorage, PeerRingAction, PeerRingRemoteAction};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::protocol::MessageSessionRelayProtocol;
use crate::message::types::{FoundVNode, Message, SearchVNode, StoreVNode, SyncVNodeWithSuccessor};
use crate::message::{MessageHandler, OriginVerificationGen};

use async_trait::async_trait;

/// TChordStorage should imply necessary method for DHT storage
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordStorage {
    /// check local cache of dht
    async fn check_cache(&self, id: &Did) -> Option<VirtualNode>;
    /// fetch virtual node from DHT
    async fn fetch(&self, id: &Did) -> Result<()>;
    /// store virtual node on DHT
    async fn store(&self, vnode: VirtualNode) -> Result<()>;
    /// Search VNode via DHT
    async fn search_vnode(&self, relay: MessageRelay<Message>, msg: SearchVNode) -> Result<()>;
    /// Remote Peer Found a vnode
    async fn found_vnode(&self, relay: MessageRelay<Message>, msg: FoundVNode) -> Result<()>;
    /// Store VNode
    async fn store_vnode(&self, relay: MessageRelay<Message>, msg: StoreVNode) -> Result<()>;
    /// Sync VNode data when successor is updated
    async fn sync_with_successor(
        &self,
        relay: MessageRelay<Message>,
        msg: SyncVNodeWithSuccessor,
    ) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordStorage for MessageHandler {
    /// Check local cache
    async fn check_cache(&self, id: &Did) -> Option<VirtualNode> {
        let dht = self.dht.lock().await;
        dht.fetch_cache(id)
    }

    /// Fetch virtual node, if exist in localstoreage, copy it to the cache,
    /// else Query Remote Node
    async fn fetch(&self, id: &Did) -> Result<()> {
        // If peer found that data is on it's localstore, copy it to the cache
        let dht = self.dht.lock().await;
        match dht.lookup(id)? {
            PeerRingAction::SomeVNode(v) => {
                dht.cache(v);
                Ok(())
            }
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, _) => {
                self.send_relay_message(MessageRelay::direct(
                    Message::SearchVNode(SearchVNode { id: *id }),
                    &self.swarm.session_manager,
                    next,
                )?)
                .await?;
                Ok(())
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }

    /// Store VirtualNode, TryInto<VirtualNode> is implementated for alot of types
    async fn store(&self, vnode: VirtualNode) -> Result<()> {
        let dht = self.dht.lock().await;
        match dht.store(vnode)? {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindAndStore(vnode)) => {
                self.send_relay_message(MessageRelay::direct(
                    Message::StoreVNode(StoreVNode { data: vec![vnode] }),
                    &self.swarm.session_manager,
                    target,
                )?)
                .await?;
                Ok(())
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }

    /// Search VNode via successor
    /// If a VNode is storead local, it will response immediately.
    async fn search_vnode(&self, relay: MessageRelay<Message>, msg: SearchVNode) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        match dht.lookup(&msg.id) {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::SomeVNode(v) => {
                    relay.relay(dht.id, None)?;
                    self.send_relay_message(relay.report(
                        Message::FoundVNode(FoundVNode { data: vec![v] }),
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

    async fn found_vnode(&self, relay: MessageRelay<Message>, msg: FoundVNode) -> Result<()> {
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
            // When query successor, store in local cache
            for datum in msg.data {
                dht.cache(datum);
            }
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

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use super::*;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::message::MessageHandler;
    use crate::prelude::RTCSdpType;
    use crate::session::SessionManager;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTrickleScheme;
    use futures::lock::Mutex;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_store_vnode() -> Result<()> {
        let stun = "stun://stun.l.google.com:19302";

        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();

        let mut v = vec![key1, key2];

        v.sort_by(|a, b| {
            if a.address() < b.address() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        (key1, key2) = (v[0], v[1]);

        println!(
            "test with key1: {:?}, key2: {:?}",
            key1.address(),
            key2.address(),
        );

        let did1 = key1.address().into();
        let did2 = key2.address().into();

        let dht1 = Arc::new(Mutex::new(PeerRing::new(did1)));
        let dht2 = Arc::new(Mutex::new(PeerRing::new(did2)));

        let sm1 = SessionManager::new_with_seckey(&key1).unwrap();
        let sm2 = SessionManager::new_with_seckey(&key2).unwrap();

        let swarm1 = Arc::new(Swarm::new(stun, key1.address(), sm1.clone()));
        let swarm2 = Arc::new(Swarm::new(stun, key2.address(), sm2.clone()));

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();

        let node1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let node2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));

        // now we connect node1 and node2

        let handshake_info1 = transport1
            .get_handshake_info(&sm1, RTCSdpType::Offer)
            .await?;

        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        let handshake_info2 = transport2
            .get_handshake_info(&sm2, RTCSdpType::Answer)
            .await?;

        let addr2 = transport1.register_remote_info(handshake_info2).await?;

        assert_eq!(addr1, key1.address());
        assert_eq!(addr2, key2.address());
        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;

        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await
            .unwrap();
        swarm2
            .register(&swarm1.address(), transport2.clone())
            .await
            .unwrap();

        assert!(swarm1.get_transport(&key2.address()).is_some());
        assert!(swarm2.get_transport(&key1.address()).is_some());

        // JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        if let Message::JoinDHT(x) = ev_1.data {
            assert_eq!(x.id, did2);
        } else {
            panic!();
        }

        let ev_2 = node2.listen_once().await.unwrap();
        if let Message::JoinDHT(x) = ev_2.data {
            assert_eq!(x.id, did1);
        } else {
            panic!();
        }
        let ev_1 = node1.listen_once().await.unwrap();
        if let Message::FindSuccessorSend(x) = ev_1.data {
            assert_eq!(x.id, did2);
            assert!(!x.for_fix);
        } else {
            panic!();
        }
        let ev_2 = node2.listen_once().await.unwrap();
        if let Message::FindSuccessorSend(x) = ev_2.data {
            assert_eq!(x.id, did1);
            assert!(!x.for_fix);
        } else {
            panic!();
        }
        let ev_1 = node1.listen_once().await.unwrap();
        if let Message::FindSuccessorReport(x) = ev_1.data {
            // for node2 there is no did is more closer to key1, so it response key1
            // and dht1 wont update
            assert!(!dht1.lock().await.successor.list().contains(&did1));
            assert_eq!(x.id, did1);
            assert!(!x.for_fix);
        } else {
            panic!();
        }
        let ev_2 = node2.listen_once().await.unwrap();
        if let Message::FindSuccessorReport(x) = ev_2.data {
            // for key1 there is no did is more closer to key1, so it response key1
            // and dht2 wont update
            assert_eq!(x.id, did2);
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // for now node1's successor is node 2, node2's successor is node 1
        // now we store adata on ndoe 2 and query it from node 1
        let data = "Across the Great Wall we can reach every corner in the world.".to_string();
        let vnode: VirtualNode = data.try_into().unwrap();
        let vid = vnode.did();
        assert!(dht1.lock().await.cache.is_empty());
        assert!(dht2.lock().await.cache.is_empty());
        assert!(node1.check_cache(&vid).await.is_none());
        assert!(node2.check_cache(&vid).await.is_none());

        // test remote store
        if vid.in_range(&did2, &did2, &did1) {
            node1.store(vnode.clone()).await.unwrap();
            // if vnode in range [node2, node1]
            // vnode should stored in node2
            let ev = node2.listen_once().await.unwrap();
            if let Message::StoreVNode(x) = ev.data {
                assert_eq!(x.data[0].did(), vid);
            } else {
                panic!();
            }
        } else {
            node2.store(vnode.clone()).await.unwrap();
            // if vnode in range [node2, node1]
            // vnode should stored in node1
            let ev = node1.listen_once().await.unwrap();
            if let Message::StoreVNode(x) = ev.data {
                assert_eq!(x.data[0].did(), vid);
            } else {
                panic!();
            }
        }
        assert!(node1.check_cache(&vid).await.is_none());
        assert!(node2.check_cache(&vid).await.is_none());
        if vid.in_range(&did2, &did2, &did1) {
            assert!(dht1.lock().await.storage.is_empty());
            assert!(!dht2.lock().await.storage.is_empty());
        } else {
            assert!(!dht1.lock().await.storage.is_empty());
            assert!(dht2.lock().await.storage.is_empty());
        }
        // test remote query
        if vid.in_range(&did2, &did2, &did1) {
            // vid is in node 2
            println!("vid is on node 2 {:?}", &did2);
            node1.fetch(&vid).await.unwrap();
            // it will send reqeust to node 2
            let ev = node2.listen_once().await.unwrap();
            // node 2 received search vnode request
            if let Message::SearchVNode(x) = ev.data {
                assert_eq!(x.id, vid);
            } else {
                panic!();
            }
            let ev = node1.listen_once().await.unwrap();
            if let Message::FoundVNode(x) = ev.data {
                assert_eq!(x.data[0].did(), vid);
            } else {
                panic!();
            }
            assert!(node1.check_cache(&vid).await.is_some());
        } else {
            // vid is in node 1
            println!("vid is on node 1 {:?}", &did1);
            node2.fetch(&vid).await.unwrap();
            let ev = node1.listen_once().await.unwrap();
            if let Message::SearchVNode(x) = ev.data {
                assert_eq!(x.id, vid);
            } else {
                panic!();
            }
            let ev = node2.listen_once().await.unwrap();
            if let Message::FoundVNode(x) = ev.data {
                assert_eq!(x.data[0].did(), vid);
            } else {
                panic!();
            }
            assert!(node2.check_cache(&vid).await.is_some());
        }

        Ok(())
    }
}
