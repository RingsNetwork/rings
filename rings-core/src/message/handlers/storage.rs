use async_trait::async_trait;

use crate::dht::vnode::VirtualNode;
use crate::dht::ChordStorage;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Error;
use crate::err::Result;
use crate::message::types::FoundVNode;
use crate::message::types::Message;
use crate::message::types::SearchVNode;
use crate::message::types::StoreVNode;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

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
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordStorage for MessageHandler {
    /// Check local cache
    async fn check_cache(&self, id: &Did) -> Option<VirtualNode> {
        let dht = self.dht.lock().await;
        dht.fetch_cache(id).await
    }

    /// Fetch virtual node, if exist in localstoreage, copy it to the cache,
    /// else Query Remote Node
    async fn fetch(&self, id: &Did) -> Result<()> {
        // If peer found that data is on it's localstore, copy it to the cache
        let dht = self.dht.lock().await;
        match dht.lookup(id).await? {
            PeerRingAction::SomeVNode(v) => {
                dht.cache(v).await;
                Ok(())
            }
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, _) => {
                self.send_direct_message(Message::SearchVNode(SearchVNode { id: *id }), next)
                    .await?;
                Ok(())
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }

    /// Store VirtualNode, TryInto<VirtualNode> is implementated for alot of types
    async fn store(&self, vnode: VirtualNode) -> Result<()> {
        let dht = self.dht.lock().await;
        match dht.store(vnode).await? {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindAndStore(vnode)) => {
                self.send_direct_message(
                    Message::StoreVNode(StoreVNode { data: vec![vnode] }),
                    target,
                )
                .await?;
                Ok(())
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SearchVNode> for MessageHandler {
    /// Search VNode via successor
    /// If a VNode is storead local, it will response immediately.
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &SearchVNode) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        match dht.lookup(&msg.id).await {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::SomeVNode(v) => {
                    relay.relay(dht.id, None)?;
                    self.send_report_message(
                        Message::FoundVNode(FoundVNode { data: vec![v] }),
                        relay,
                    )
                    .await
                }
                PeerRingAction::RemoteAction(next, _) => {
                    relay.relay(dht.id, Some(next))?;
                    self.transpond_payload(ctx, relay).await
                }
                act => Err(Error::PeerRingUnexpectedAction(act)),
            },
            Err(e) => Err(e),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FoundVNode> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &FoundVNode) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        if relay.next_hop.is_some() {
            self.transpond_payload(ctx, relay).await
        } else {
            // When query successor, store in local cache
            for datum in msg.data.iter().cloned() {
                dht.cache(datum).await;
            }
            Ok(())
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<StoreVNode> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &StoreVNode) -> Result<()> {
        let dht = self.dht.lock().await;

        let virtual_peer = msg.data.clone();
        for p in virtual_peer {
            match dht.store(p).await {
                Ok(action) => match action {
                    PeerRingAction::None => Ok(()),
                    PeerRingAction::RemoteAction(next, _) => {
                        let mut relay = ctx.relay.clone();
                        relay.reset_destination(next)?;
                        relay.relay(dht.id, Some(next))?;
                        self.transpond_payload(ctx, relay).await
                    }
                    act => Err(Error::PeerRingUnexpectedAction(act)),
                },
                Err(e) => Err(e),
            }?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SyncVNodeWithSuccessor> for MessageHandler {
    // received remote sync vnode request
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &SyncVNodeWithSuccessor,
    ) -> Result<()> {
        let dht = self.dht.lock().await;

        for data in msg.data.iter().cloned() {
            // only simply store here
            match dht.store(data).await {
                Ok(PeerRingAction::None) => Ok(()),
                Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindAndStore(peer),
                )) => {
                    self.send_direct_message(
                        Message::StoreVNode(StoreVNode { data: vec![peer] }),
                        next,
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

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use futures::lock::Mutex;

    use super::*;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::message::MessageHandler;
    use crate::prelude::RTCSdpType;
    use crate::session::SessionManager;
    use crate::storage::PersistenceStorageOperation;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTrickleScheme;

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

        let dht1 = Arc::new(Mutex::new(PeerRing::new(did1).await.unwrap()));
        let dht2 = Arc::new(Mutex::new(PeerRing::new(did2).await.unwrap()));

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
            assert!(dht1.lock().await.storage.count().await.unwrap() == 0);
            assert!(!dht2.lock().await.storage.count().await.unwrap() == 0);
        } else {
            assert!(!dht1.lock().await.storage.count().await.unwrap() == 0);
            assert!(dht2.lock().await.storage.count().await.unwrap() == 0);
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
