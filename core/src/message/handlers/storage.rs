#![warn(missing_docs)]
use async_trait::async_trait;

use crate::consts::VNODE_DATA_REDUNDANT;
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
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::Encoded;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::prelude::vnode::VNodeOperation;
use crate::swarm::Swarm;

/// ChordStorageInterface should imply necessary method for DHT storage
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageInterface {
    /// check local cache of dht
    async fn storage_check_cache(&self, vid: Did) -> Option<VirtualNode>;
    /// fetch virtual node from DHT
    async fn storage_fetch(&self, vid: Did) -> Result<()>;
    /// store virtual node on DHT
    async fn storage_store(&self, vnode: VirtualNode) -> Result<()>;
    /// append data to Data type virtual node
    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()>;
    /// append data to Data type virtual node uniquely
    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageInterface for Swarm {
    /// Check local cache
    async fn storage_check_cache(&self, vid: Did) -> Option<VirtualNode> {
        self.dht.local_cache_get(vid)
    }

    /// Fetch virtual node, if exist in localstoreage, copy it to the cache,
    /// else Query Remote Node
    async fn storage_fetch(&self, vid: Did) -> Result<()> {
        // If peer found that data is on it's localstore, copy it to the cache
        for vid in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            match self.dht.vnode_lookup(vid).await? {
                PeerRingAction::None => (),
                PeerRingAction::SomeVNode(v) => {
                    self.dht.local_cache_set(v);
                }
                PeerRingAction::RemoteAction(next, _) => {
                    tracing::debug!(
                        "storage_fetch send_message: SearchVNode({:?}) to {:?}",
                        vid,
                        next
                    );
                    self.send_message(Message::SearchVNode(SearchVNode { vid }), next)
                        .await?;
                }
                act => return Err(Error::PeerRingUnexpectedAction(act)),
            }
        }
        Ok(())
    }

    /// Store VirtualNode, `TryInto<VirtualNode>` is implemented for alot of types
    async fn storage_store(&self, vnode: VirtualNode) -> Result<()> {
        for vnode in vnode.affine(VNODE_DATA_REDUNDANT) {
            let op = VNodeOperation::Overwrite(vnode);
            match self.dht.vnode_operate(op).await? {
                PeerRingAction::None => (),
                PeerRingAction::RemoteAction(
                    target,
                    PeerRingRemoteAction::FindVNodeForOperate(op),
                ) => {
                    self.send_message(Message::OperateVNode(op), target).await?;
                }
                act => return Err(Error::PeerRingUnexpectedAction(act)),
            }
        }
        Ok(())
    }

    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let vnode: VirtualNode = (topic.to_string(), data).try_into()?;
        for vnode in vnode.affine(VNODE_DATA_REDUNDANT) {
            let op = VNodeOperation::Extend(vnode);
            match self.dht.vnode_operate(op).await? {
                PeerRingAction::None => (),
                PeerRingAction::RemoteAction(
                    target,
                    PeerRingRemoteAction::FindVNodeForOperate(op),
                ) => {
                    self.send_message(Message::OperateVNode(op), target).await?;
                }
                act => return Err(Error::PeerRingUnexpectedAction(act)),
            }
        }
        return Ok(());
    }

    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let vnode: VirtualNode = (topic.to_string(), data).try_into()?;
        for vnode in vnode.affine(VNODE_DATA_REDUNDANT) {
            let op = VNodeOperation::Touch(vnode);

            match self.dht.vnode_operate(op).await? {
                PeerRingAction::None => (),
                PeerRingAction::RemoteAction(
                    target,
                    PeerRingRemoteAction::FindVNodeForOperate(op),
                ) => {
                    self.send_message(Message::OperateVNode(op), target).await?;
                }
                act => return Err(Error::PeerRingUnexpectedAction(act)),
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SearchVNode> for MessageHandler {
    /// Search VNode via successor
    /// If a VNode is storead local, it will response immediately.
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &SearchVNode) -> Result<()> {
        match self.dht.vnode_lookup(msg.vid).await {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::SomeVNode(v) => {
                    self.send_report_message(ctx, Message::FoundVNode(FoundVNode { data: vec![v] }))
                        .await
                }
                PeerRingAction::RemoteAction(next, _) => self.reset_destination(ctx, next).await,
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
        if self.dht.did != ctx.relay.destination {
            return self.forward_payload(ctx, None).await;
        }
        for data in msg.data.iter().cloned() {
            self.dht.local_cache_set(data);
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<VNodeOperation> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &VNodeOperation) -> Result<()> {
        match self.dht.vnode_operate(msg.clone()).await {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::RemoteAction(next, _) => self.reset_destination(ctx, next).await,
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
    ) -> Result<()> {
        for data in msg.data.iter().cloned() {
            // only simply store here
            self.swarm.storage_store(data).await?;
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
    use crate::message::Encoder;
    use crate::prelude::vnode::VNodeType;
    use crate::storage::PersistenceStorageOperation;
    use crate::tests::default::prepare_node;

    #[tokio::test]
    async fn test_store_vnode() -> Result<()> {
        let keys = gen_ordered_keys(2);
        let (key1, key2) = (keys[0], keys[1]);
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        // Now, node1 is the successor of node2, and node2 is the successor of node1.
        // Following tests storing data on ndoe2 and query it from node1.
        let data = "Across the Great Wall we can reach every corner in the world.".to_string();
        let vnode: VirtualNode = data.clone().try_into().unwrap();
        let vid = vnode.did;

        // Make sure the data is stored on node2.
        let ((_did1, dht1, swarm1, node1), (did2, dht2, swarm2, node2)) =
            if vid.in_range(did2, did2, did1) {
                ((did1, dht1, swarm1, node1), (did2, dht2, swarm2, node2))
            } else {
                ((did2, dht2, swarm2, node2), (did1, dht1, swarm1, node1))
            };

        assert!(dht1.cache.is_empty());
        assert!(dht2.cache.is_empty());
        assert!(swarm1.storage_check_cache(vid).await.is_none());
        assert!(swarm2.storage_check_cache(vid).await.is_none());

        swarm1.storage_store(vnode.clone()).await.unwrap();
        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Overwrite(x)) if x.did == i
                ));
        }

        assert!(swarm1.storage_check_cache(vid).await.is_none());
        assert!(swarm2.storage_check_cache(vid).await.is_none());
        assert!(dht1.storage.count().await.unwrap() == 0);
        assert!(dht2.storage.count().await.unwrap() != 0);

        // test remote query
        println!("vid is on node2 {:?}", &did2);
        swarm1.storage_fetch(vid).await.unwrap();

        // it will send request to node2
        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            // node2 received search vnode request
            assert!(matches!(
            ev.data,
            Message::SearchVNode(x) if x.vid == i
                ));
        }

        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node1.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::FoundVNode(x) if x.data[0].did == i
                ));
        }

        assert_eq!(
            swarm1.storage_check_cache(vid).await,
            Some(VirtualNode {
                did: vid,
                data: vec![data.encode()?],
                kind: VNodeType::Data
            })
        );

        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok(())
    }

    #[tokio::test]
    async fn test_extend_data() -> Result<()> {
        let keys = gen_ordered_keys(2);
        let (key1, key2) = (keys[0], keys[1]);
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        // Now, node1 is the successor of node2, and node2 is the successor of node1.
        // Following tests storing data on ndoe2 and query it from node1.
        let topic = "Across the Great Wall we can reach every corner in the world.".to_string();
        let vnode: VirtualNode = topic.clone().try_into().unwrap();
        let vid = vnode.did;

        // Make sure the data is stored on node2.
        let ((_did1, dht1, swarm1, node1), (did2, dht2, swarm2, node2)) =
            if vid.in_range(did2, did2, did1) {
                ((did1, dht1, swarm1, node1), (did2, dht2, swarm2, node2))
            } else {
                ((did2, dht2, swarm2, node2), (did1, dht1, swarm1, node1))
            };

        assert!(dht1.cache.is_empty());
        assert!(dht2.cache.is_empty());
        assert!(swarm1.storage_check_cache(vid).await.is_none());
        assert!(swarm2.storage_check_cache(vid).await.is_none());

        swarm1
            .storage_append_data(&topic, "111".to_string().encode()?)
            .await
            .unwrap();

        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Extend(VirtualNode { did, data, kind: VNodeType::Data }))
                if did == i && data == vec!["111".to_string().encode()?]
                ));
        }
        swarm1
            .storage_append_data(&topic, "222".to_string().encode()?)
            .await
            .unwrap();

        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Extend(VirtualNode { did, data, kind: VNodeType::Data }))
                if did == i && data == vec!["222".to_string().encode()?]
                ));
        }
        assert!(swarm1.storage_check_cache(vid).await.is_none());
        assert!(swarm2.storage_check_cache(vid).await.is_none());
        assert!(dht1.storage.count().await.unwrap() == 0);
        assert!(dht2.storage.count().await.unwrap() != 0);

        // test remote query
        println!("vid is on node2 {:?}", &did2);
        swarm1.storage_fetch(vid).await.unwrap();

        // it will send request to node2
        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            // node2 received search vnode request
            assert!(matches!(
            ev.data,
            Message::SearchVNode(x) if x.vid == i
                ));
        }

        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node1.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::FoundVNode(x) if x.data[0].did == i
                ));
        }

        assert_eq!(
            swarm1.storage_check_cache(vid).await,
            Some(VirtualNode {
                did: vid,
                data: vec!["111".to_string().encode()?, "222".to_string().encode()?],
                kind: VNodeType::Data
            })
        );

        // Append more data
        swarm1
            .storage_append_data(&topic, "333".to_string().encode()?)
            .await
            .unwrap();

        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::OperateVNode(VNodeOperation::Extend(VirtualNode { did, data, kind: VNodeType::Data }))
                if did == i && data == vec!["333".to_string().encode()?]
                ));
        }

        // test remote query agagin
        println!("vid is on node2 {:?}", &did2);
        swarm1.storage_fetch(vid).await.unwrap();

        // it will send request to node2
        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node2.listen_once().await.unwrap();
            // node2 received search vnode request
            assert!(matches!(
            ev.data,
            Message::SearchVNode(x) if x.vid == i
                ));
        }

        for i in vid.rotate_affine(VNODE_DATA_REDUNDANT) {
            let ev = node1.listen_once().await.unwrap();
            assert!(matches!(
            ev.data,
            Message::FoundVNode(x) if x.data[0].did == i
                ));
        }

        assert_eq!(
            swarm1.storage_check_cache(vid).await,
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
