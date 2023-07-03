#![warn(missing_docs)]
use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::dht::vnode::VirtualNode;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::FoundVNode;
use crate::message::types::Message;
use crate::message::types::SearchVNode;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::Encoded;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::prelude::vnode::VNodeOperation;
use crate::swarm::Swarm;

/// ChordStorageInterface should imply necessary method for DHT storage
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageInterface<const REDUNDANT: u16> {
    /// fetch virtual node from DHT
    async fn storage_fetch(&self, vid: Did) -> Result<()>;
    /// store virtual node on DHT
    async fn storage_store(&self, vnode: VirtualNode) -> Result<()>;
    /// append data to Data type virtual node
    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()>;
    /// append data to Data type virtual node uniquely
    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()>;
}

/// ChordStorageInterfaceCacheChecker defines the interface for checking the local cache of the DHT.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageInterfaceCacheChecker {
    // Check the local cache of the DHT for a specific virtual node identifier (vid).
    ///
    /// Returns an optional `VirtualNode` representing the cached data, or `None` if it is not found.
    async fn storage_check_cache(&self, vid: Did) -> Option<VirtualNode>;
}

/// Handle the storage fetch action of the peer ring.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_fetch_act(swarm: &Swarm, act: PeerRingAction) -> Result<()> {
    match act {
        PeerRingAction::None => (),
        PeerRingAction::SomeVNode(v) => {
            swarm.dht.local_cache_set(v);
        }
        PeerRingAction::RemoteAction(next, dht_act) => {
            if let PeerRingRemoteAction::FindVNode(vid) = dht_act {
                tracing::debug!(
                    "storage_fetch send_message: SearchVNode({:?}) to {:?}",
                    vid,
                    next
                );
                swarm
                    .send_message(Message::SearchVNode(SearchVNode { vid }), next)
                    .await?;
            }
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_fetch_act(swarm, act).await?;
            }
        }
        act => return Err(Error::PeerRingUnexpectedAction(act)),
    }
    Ok(())
}

/// Handle the storage store operations of the peer ring.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub(super) async fn handle_storage_store_act(swarm: &Swarm, act: PeerRingAction) -> Result<()> {
    match act {
        PeerRingAction::None => (),
        PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindVNodeForOperate(op)) => {
            swarm
                .send_message(Message::OperateVNode(op), target)
                .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_store_act(swarm, act).await?;
            }
        }
        act => return Err(Error::PeerRingUnexpectedAction(act)),
    }
    Ok(())
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageInterfaceCacheChecker for Swarm {
    /// Check local cache
    async fn storage_check_cache(&self, vid: Did) -> Option<VirtualNode> {
        self.dht.local_cache_get(vid)
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl<const REDUNDANT: u16> ChordStorageInterface<REDUNDANT> for Swarm {
    /// Fetch virtual node, if exist in localstoreage, copy it to the cache,
    /// else Query Remote Node
    async fn storage_fetch(&self, vid: Did) -> Result<()> {
        // If peer found that data is on it's localstore, copy it to the cache
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::vnode_lookup(&self.dht, vid).await?;
        handle_storage_fetch_act(self, act).await?;
        Ok(())
    }

    /// Store VirtualNode, `TryInto<VirtualNode>` is implemented for alot of types
    async fn storage_store(&self, vnode: VirtualNode) -> Result<()> {
        let op = VNodeOperation::Overwrite(vnode);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::vnode_operate(&self.dht, op).await?;
        handle_storage_store_act(self, act).await?;
        Ok(())
    }

    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let vnode: VirtualNode = (topic.to_string(), data).try_into()?;
        let op = VNodeOperation::Extend(vnode);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::vnode_operate(&self.dht, op).await?;
        handle_storage_store_act(self, act).await?;
        Ok(())
    }

    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let vnode: VirtualNode = (topic.to_string(), data).try_into()?;
        let op = VNodeOperation::Touch(vnode);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::vnode_operate(&self.dht, op).await?;
        handle_storage_store_act(self, act).await?;
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SearchVNode> for MessageHandler {
    /// Search VNode via successor
    /// If a VNode is storead local, it will response immediately.(See Chordstorageinterface::storage_fetch)
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &SearchVNode,
    ) -> Result<Vec<MessageHandlerEvent>> {
        // For relay message, set redundant to 1
        match <PeerRing as ChordStorage<_, 1>>::vnode_lookup(&self.dht, msg.vid).await {
            Ok(action) => match action {
                PeerRingAction::None => Ok(vec![]),
                PeerRingAction::SomeVNode(v) => Ok(vec![MessageHandlerEvent::SendReportMessage(
                    Message::FoundVNode(FoundVNode { data: vec![v] }),
                )]),
                PeerRingAction::RemoteAction(next, _) => {
                    Ok(vec![MessageHandlerEvent::ResetDestination(next)])
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
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &FoundVNode,
    ) -> Result<Vec<MessageHandlerEvent>> {
        if self.dht.did != ctx.relay.destination {
            return Ok(vec![MessageHandlerEvent::ForwardPayload(None)]);
        }
        for data in msg.data.iter().cloned() {
            self.dht.local_cache_set(data);
        }
        Ok(vec![])
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<VNodeOperation> for MessageHandler {
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &VNodeOperation,
    ) -> Result<Vec<MessageHandlerEvent>> {
        // For relay message, set redundant to 1
        match <PeerRing as ChordStorage<_, 1>>::vnode_operate(&self.dht, msg.clone()).await {
            Ok(action) => match action {
                PeerRingAction::None => Ok(vec![]),
                PeerRingAction::RemoteAction(next, _) => {
                    Ok(vec![MessageHandlerEvent::ResetDestination(next)])
                }
                act => Err(Error::PeerRingUnexpectedAction(act)),
            },
            Err(e) => Err(e),
        }
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
    ) -> Result<Vec<MessageHandlerEvent>> {
        let mut events = vec![];
        for data in msg.data.iter().cloned() {
            // only simply store here
            // For relay message, set redundant to 1
            events.push(MessageHandlerEvent::StorageStore(data));
        }
        Ok(events)
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use super::*;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::message::handlers::connection::tests::test_only_two_nodes_establish_connection;
    use crate::message::Encoder;
    use crate::prelude::vnode::VNodeType;
    use crate::storage::PersistenceStorageOperation;
    use crate::tests::default::prepare_node;

    #[tokio::test]
    async fn test_store_vnode() -> Result<()> {
        let keys = gen_ordered_keys(2);
        let (key1, key2) = (keys[0], keys[1]);
        let (node1, _path1) = prepare_node(key1).await;
        let (node2, _path2) = prepare_node(key2).await;
        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        // Now, node1 is the successor of node2, and node2 is the successor of node1.
        // Following tests storing data on node2 and query it from node1.
        let data = "Across the Great Wall we can reach every corner in the world.".to_string();
        let vnode: VirtualNode = data.clone().try_into().unwrap();
        let vid = vnode.did;

        // Make sure the data is stored on node2.
        let (node1, node2) = if vid.in_range(node2.did(), node2.did(), node1.did()) {
            (node1, node2)
        } else {
            (node2, node1)
        };

        assert!(node1.dht().cache.is_empty());
        assert!(node2.dht().cache.is_empty());
        assert!(node1.storage_check_cache(vid).await.is_none());
        assert!(node2.storage_check_cache(vid).await.is_none());

        <Swarm as ChordStorageInterface<1>>::storage_store(&node1, vnode.clone())
            .await
            .unwrap();
        let ev = node2.listen_once().await.unwrap().0;
        assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Overwrite(x)) if x.did == vid
        ));

        assert!(node1.storage_check_cache(vid).await.is_none());
        assert!(node2.storage_check_cache(vid).await.is_none());
        assert!(node1.dht().storage.count().await.unwrap() == 0);
        assert!(node2.dht().storage.count().await.unwrap() != 0);

        // test remote query
        println!("vid is on node2 {:?}", node2.did());
        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1, vid)
            .await
            .unwrap();

        // it will send request to node2
        let ev = node2.listen_once().await.unwrap().0;
        // node2 received search vnode request
        assert!(matches!(
            ev.data,
            Message::SearchVNode(x) if x.vid == vid
        ));

        let ev = node1.listen_once().await.unwrap().0;
        assert!(matches!(
            ev.data,
            Message::FoundVNode(x) if x.data[0].did == vid
        ));

        assert_eq!(
            node1.storage_check_cache(vid).await,
            Some(VirtualNode {
                did: vid,
                data: vec![data.encode()?],
                kind: VNodeType::Data
            })
        );

        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok(())
    }

    #[cfg(not(feature = "redundant"))]
    #[tokio::test]
    async fn test_extend_data() -> Result<()> {
        let keys = gen_ordered_keys(2);
        let (key1, key2) = (keys[0], keys[1]);
        let (node1, _path1) = prepare_node(key1).await;
        let (node2, _path2) = prepare_node(key2).await;
        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        // Now, node1 is the successor of node2, and node2 is the successor of node1.
        // Following tests storing data on node2 and query it from node1.
        let topic = "Across the Great Wall we can reach every corner in the world.".to_string();
        let vnode: VirtualNode = topic.clone().try_into().unwrap();
        let vid = vnode.did;

        // Make sure the data is stored on node2.
        let (node1, node2) = if vid.in_range(node2.did(), node2.did(), node1.did()) {
            (node1, node2)
        } else {
            (node2, node1)
        };

        assert!(node1.dht().cache.is_empty());
        assert!(node2.dht().cache.is_empty());
        assert!(node1.storage_check_cache(vid).await.is_none());
        assert!(node2.storage_check_cache(vid).await.is_none());

        <Swarm as ChordStorageInterface<1>>::storage_append_data(
            &node1,
            &topic,
            "111".to_string().encode()?,
        )
        .await
        .unwrap();

        let ev = node2.listen_once().await.unwrap().0;
        assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Extend(VirtualNode { did, data, kind: VNodeType::Data }))
                if did == vid && data == vec!["111".to_string().encode()?]
        ));
        <Swarm as ChordStorageInterface<1>>::storage_append_data(
            &node1,
            &topic,
            "222".to_string().encode()?,
        )
        .await
        .unwrap();
        let ev = node2.listen_once().await.unwrap().0;
        assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Extend(VirtualNode { did, data, kind: VNodeType::Data }))
                if did == vid && data == vec!["222".to_string().encode()?]
        ));
        assert!(node1.storage_check_cache(vid).await.is_none());
        assert!(node2.storage_check_cache(vid).await.is_none());
        assert!(node1.dht().storage.count().await.unwrap() == 0);
        assert!(node2.dht().storage.count().await.unwrap() != 0);
        // test remote query
        println!("vid is on node2 {:?}", node2.did());
        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1, vid)
            .await
            .unwrap();

        // it will send request to node2
        let ev = node2.listen_once().await.unwrap().0;

        // node2 received search vnode request
        assert!(matches!(
            ev.data,
            Message::SearchVNode(x) if x.vid == vid
        ));
        let ev = node1.listen_once().await.unwrap().0;

        assert!(matches!(
            ev.data,
            Message::FoundVNode(x) if x.data[0].did == vid
        ));

        assert_eq!(
            node1.storage_check_cache(vid).await,
            Some(VirtualNode {
                did: vid,
                data: vec!["111".to_string().encode()?, "222".to_string().encode()?],
                kind: VNodeType::Data
            })
        );

        // Append more data
        <Swarm as ChordStorageInterface<1>>::storage_append_data(
            &node1,
            &topic,
            "333".to_string().encode()?,
        )
        .await
        .unwrap();

        let ev = node2.listen_once().await.unwrap().0;
        assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Extend(VirtualNode { did, data, kind: VNodeType::Data }))
                if did == vid && data == vec!["333".to_string().encode()?]
        ));

        // test remote query agagin
        println!("vid is on node2 {:?}", node2.did());
        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1, vid)
            .await
            .unwrap();

        // it will send request to node2
        let ev = node2.listen_once().await.unwrap().0;
        // node2 received search vnode request
        assert!(matches!(
            ev.data,
            Message::SearchVNode(x) if x.vid == vid
        ));

        let ev = node1.listen_once().await.unwrap().0;
        assert!(matches!(
            ev.data,
            Message::FoundVNode(x) if x.data[0].did == vid
        ));

        assert_eq!(
            node1.storage_check_cache(vid).await,
            Some(VirtualNode {
                did: vid,
                data: vec![
                    "111".to_string().encode()?,
                    "222".to_string().encode()?,
                    "333".to_string().encode()?
                ],
                kind: VNodeType::Data
            })
        );

        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok(())
    }
}
