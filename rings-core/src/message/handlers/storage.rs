#![warn(missing_docs)]
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
use crate::swarm::Swarm;

/// TChordStorage should imply necessary method for DHT storage
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordStorage {
    /// check local cache of dht
    async fn storage_check_cache(&self, vid: Did) -> Option<VirtualNode>;
    /// fetch virtual node from DHT
    async fn storage_fetch(&self, vid: Did) -> Result<()>;
    /// store virtual node on DHT
    async fn storage_store(&self, vnode: VirtualNode) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordStorage for Swarm {
    /// Check local cache
    async fn storage_check_cache(&self, vid: Did) -> Option<VirtualNode> {
        self.dht.local_cache_get(vid)
    }

    /// Fetch virtual node, if exist in localstoreage, copy it to the cache,
    /// else Query Remote Node
    async fn storage_fetch(&self, vid: Did) -> Result<()> {
        // If peer found that data is on it's localstore, copy it to the cache
        match self.dht.lookup(vid).await? {
            PeerRingAction::SomeVNode(v) => {
                self.dht.local_cache_set(v);
                Ok(())
            }
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, _) => {
                self.send_direct_message(Message::SearchVNode(SearchVNode { vid }), next)
                    .await?;
                Ok(())
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }

    /// Store VirtualNode, TryInto<VirtualNode> is implementated for alot of types
    async fn storage_store(&self, vnode: VirtualNode) -> Result<()> {
        match self.dht.store(vnode).await? {
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
        let mut relay = ctx.relay.clone();

        match self.dht.lookup(msg.vid).await {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::SomeVNode(v) => {
                    relay.relay(self.dht.did, None)?;
                    self.send_report_message(
                        Message::FoundVNode(FoundVNode { data: vec![v] }),
                        ctx.tx_id,
                        relay,
                    )
                    .await
                }
                PeerRingAction::RemoteAction(next, _) => {
                    relay.relay(self.dht.did, Some(next))?;
                    self.forward_payload(ctx, relay).await
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
        let mut relay = ctx.relay.clone();

        relay.relay(self.dht.did, None)?;
        if relay.next_hop.is_some() {
            self.forward_payload(ctx, relay).await
        } else {
            // When query successor, store in local cache
            for datum in msg.data.iter().cloned() {
                self.dht.local_cache_set(datum);
            }
            Ok(())
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<StoreVNode> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &StoreVNode) -> Result<()> {
        let virtual_peer = msg.data.clone();
        for p in virtual_peer {
            match self.dht.store(p).await {
                Ok(action) => match action {
                    PeerRingAction::None => Ok(()),
                    PeerRingAction::RemoteAction(next, _) => {
                        let mut relay = ctx.relay.clone();
                        relay.reset_destination(next)?;
                        relay.relay(self.dht.did, Some(next))?;
                        self.forward_payload(ctx, relay).await
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
        for data in msg.data.iter().cloned() {
            // only simply store here
            match self.dht.store(data).await {
                Ok(PeerRingAction::None) => Ok(()),
                Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindAndStore(peer),
                )) => {
                    self.send_direct_message(
                        Message::StoreVNode(StoreVNode { data: vec![peer] }),
                        next,
                    )
                    .await?;
                    Ok(())
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
    use crate::ecc::tests::gen_ordered_keys;
    use crate::message::handlers::connection::tests::test_only_two_nodes_establish_connection;
    use crate::storage::PersistenceStorageOperation;
    use crate::tests::default::prepare_node;

    #[tokio::test]
    async fn test_store_vnode() -> Result<()> {
        let keys = gen_ordered_keys(2);
        let (key1, key2) = (keys[0], keys[1]);
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        // for now node1's successor is node 2, node2's successor is node 1
        // now we store adata on ndoe 2 and query it from node 1
        let data = "Across the Great Wall we can reach every corner in the world.".to_string();
        let vnode: VirtualNode = data.try_into().unwrap();
        let vid = vnode.did;
        assert!(dht1.cache.is_empty());
        assert!(dht2.cache.is_empty());
        assert!(swarm1.storage_check_cache(vid).await.is_none());
        assert!(swarm2.storage_check_cache(vid).await.is_none());

        // test remote store
        if vid.in_range(&did2, &did2, &did1) {
            swarm1.storage_store(vnode.clone()).await.unwrap();
            // if vnode in range [node2, node1]
            // vnode should stored in node2
            let ev = node2.listen_once().await.unwrap();
            if let Message::StoreVNode(x) = ev.data {
                assert_eq!(x.data[0].did, vid);
            } else {
                panic!();
            }
        } else {
            swarm2.storage_store(vnode.clone()).await.unwrap();
            // if vnode in range [node2, node1]
            // vnode should stored in node1
            let ev = node1.listen_once().await.unwrap();
            if let Message::StoreVNode(x) = ev.data {
                assert_eq!(x.data[0].did, vid);
            } else {
                panic!();
            }
        }
        assert!(swarm1.storage_check_cache(vid).await.is_none());
        assert!(swarm2.storage_check_cache(vid).await.is_none());
        if vid.in_range(&did2, &did2, &did1) {
            assert!(dht1.storage.count().await.unwrap() == 0);
            assert!(dht2.storage.count().await.unwrap() != 0);
        } else {
            assert!(dht1.storage.count().await.unwrap() != 0);
            assert!(dht2.storage.count().await.unwrap() == 0);
        }
        // test remote query
        if vid.in_range(&did2, &did2, &did1) {
            // vid is in node 2
            println!("vid is on node 2 {:?}", &did2);
            swarm1.storage_fetch(vid).await.unwrap();
            // it will send reqeust to node 2
            let ev = node2.listen_once().await.unwrap();
            // node 2 received search vnode request
            if let Message::SearchVNode(x) = ev.data {
                assert_eq!(x.vid, vid);
            } else {
                panic!();
            }
            let ev = node1.listen_once().await.unwrap();
            if let Message::FoundVNode(x) = ev.data {
                assert_eq!(x.data[0].did, vid);
            } else {
                panic!();
            }
            assert!(swarm1.storage_check_cache(vid).await.is_some());
        } else {
            // vid is in node 1
            println!("vid is on node 1 {:?}", &did1);
            swarm2.storage_fetch(vid).await.unwrap();
            let ev = node1.listen_once().await.unwrap();
            if let Message::SearchVNode(x) = ev.data {
                assert_eq!(x.vid, vid);
            } else {
                panic!();
            }
            let ev = node2.listen_once().await.unwrap();
            if let Message::FoundVNode(x) = ev.data {
                assert_eq!(x.data[0].did, vid);
            } else {
                panic!();
            }
            assert!(swarm2.storage_check_cache(vid).await.is_some());
        }

        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok(())
    }
}
