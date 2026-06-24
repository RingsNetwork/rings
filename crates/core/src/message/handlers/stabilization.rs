use async_trait::async_trait;

use crate::dht::Chord;
use crate::dht::ChordStorageSync;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Result;
use crate::message::effects::ConnectionFunctor;
use crate::message::effects::MessageSendFunctor;
use crate::message::effects::PayloadRelayFunctor;
use crate::message::types::Message;
use crate::message::types::NotifyPredecessorReport;
use crate::message::types::NotifyPredecessorSend;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &NotifyPredecessorSend) -> Result<()> {
        let predecessor = self.dht.notify(msg.did)?;

        if predecessor != ctx.relay.origin_sender() {
            return self
                .run_effects([PayloadRelayFunctor::send_report_message(
                    ctx,
                    Message::NotifyPredecessorReport(NotifyPredecessorReport { did: predecessor }),
                )
                .into()])
                .await;
        }

        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorReport> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload, msg: &NotifyPredecessorReport) -> Result<()> {
        self.run_effects([ConnectionFunctor::connect_dht_peer(msg.did).into()])
            .await?;

        if let Ok(PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
        )) = self.dht.sync_vnode_with_successor(msg.did).await
        {
            self.run_effects([MessageSendFunctor::send_message(
                Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                next,
            )
            .into()])
                .await?;
        }

        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use tokio::time::timeout;
    use tokio::time::Duration;

    use super::*;
    use crate::dht::successor::SuccessorReader;
    use crate::dht::vnode::VNodeType;
    use crate::dht::vnode::VirtualNode;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::message::Encoder;
    use crate::session::SessionSk;
    use crate::swarm::callback::SwarmCallback;
    use crate::swarm::Swarm;
    use crate::tests::default::assert_no_more_msg;
    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::manually_establish_connection;

    struct NoopCallback;

    impl SwarmCallback for NoopCallback {}

    fn next_generated_key(keys: &mut impl Iterator<Item = SecretKey>) -> Result<SecretKey> {
        keys.next()
            .ok_or_else(|| Error::InvalidMessage("expected generated key".to_string()))
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_1_2_3() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_stabilization(key1, key2, key3).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_2_3_1() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);

        test_triple_ordered_nodes_stabilization(key2, key3, key1).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_3_1_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_stabilization(key3, key1, key2).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_3_2_1() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_2_1_3() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_stabilization(key2, key1, key3).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_1_3_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_stabilization(key1, key3, key2).await
    }

    #[tokio::test]
    async fn notify_predecessor_report_syncs_vnodes_when_predecessor_already_connected(
    ) -> Result<()> {
        let mut keys = gen_ordered_keys(3).into_iter();
        let key1 = next_generated_key(&mut keys)?;
        let key2 = next_generated_key(&mut keys)?;
        let key3 = next_generated_key(&mut keys)?;

        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;
        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        let vnode = VirtualNode {
            did: key3.address().into(),
            data: vec![String::from("sync me").encode()?],
            kind: VNodeType::Data,
        };
        node1
            .dht()
            .storage
            .put(&vnode.did.to_string(), &vnode)
            .await?;

        let context_key = SecretKey::random();
        let context_session = SessionSk::new_with_seckey(&context_key)?;
        let context = MessagePayload::new_send(
            Message::custom(b"notify report context")?,
            &context_session,
            node1.did(),
            node1.did(),
        )?;

        let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
        handler
            .handle(&context, &NotifyPredecessorReport { did: node2.did() })
            .await?;

        let payload = match timeout(Duration::from_secs(1), node2.listen_once()).await {
            Ok(Some(payload)) => payload,
            Ok(None) => {
                return Err(Error::InvalidMessage(
                    "node2 message stream closed before vnode sync".to_string(),
                ))
            }
            Err(_) => {
                return Err(Error::InvalidMessage(
                    "timed out waiting for vnode sync".to_string(),
                ))
            }
        };

        match payload.transaction.data::<Message>()? {
            Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }) => {
                assert_eq!(data, vec![vnode]);
            }
            message => {
                return Err(Error::InvalidMessage(format!(
                    "expected SyncVNodeWithSuccessor, got {message:?}"
                )))
            }
        }
        assert_eq!(node1.dht().storage.count().await?, 0);

        Ok(())
    }

    async fn test_triple_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;
        let node3 = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state before stabilization ===");
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node1.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);
        assert!(node1.dht().lock_predecessor()?.is_none());
        assert!(node2.dht().lock_predecessor()?.is_none());
        assert!(node3.dht().lock_predecessor()?.is_none());

        println!("========================================");
        println!("||  now we start first stabilization  ||");
        println!("========================================");

        run_stabilization_once(node1.swarm.clone()).await?;
        run_stabilization_once(node2.swarm.clone()).await?;
        run_stabilization_once(node3.swarm.clone()).await?;

        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state after first stabilization ===");
        assert!(node1.dht().successors().list()?.contains(&node2.did()));
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node1.did()
        ]);
        assert!(node3.dht().successors().list()?.contains(&node2.did()));

        println!("==========================================");
        println!("||  now we start 5 times stabilization  ||");
        println!("==========================================");

        for _ in 0..5 {
            run_stabilization_once(node1.swarm.clone()).await?;
            run_stabilization_once(node2.swarm.clone()).await?;
            run_stabilization_once(node3.swarm.clone()).await?;

            wait_for_msgs([&node1, &node2, &node3]).await;
            assert_no_more_msg([&node1, &node2, &node3]).await;

            println!("=== Check state after stabilization ===");
            assert_eq!(node1.dht().successors().list()?, vec![
                node2.did(),
                node3.did()
            ]);
            assert_eq!(node2.dht().successors().list()?, vec![
                node3.did(),
                node1.did()
            ]);
            assert_eq!(node3.dht().successors().list()?, vec![
                node1.did(),
                node2.did()
            ]);
        }

        println!("=== Check predecessor after all stabilization ===");
        assert_eq!(*node1.dht().lock_predecessor()?, Some(node3.did()));
        assert_eq!(*node2.dht().lock_predecessor()?, Some(node1.did()));
        assert_eq!(*node3.dht().lock_predecessor()?, Some(node2.did()));
        Ok(())
    }

    async fn test_triple_desc_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;
        let node3 = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state before stabilization ===");
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);
        assert!(node1.dht().lock_predecessor()?.is_none());
        assert!(node2.dht().lock_predecessor()?.is_none());
        assert!(node3.dht().lock_predecessor()?.is_none());

        println!("========================================");
        println!("||  now we start first stabilization  ||");
        println!("========================================");

        run_stabilization_once(node1.swarm.clone()).await?;
        run_stabilization_once(node2.swarm.clone()).await?;
        run_stabilization_once(node3.swarm.clone()).await?;

        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state after first stabilization ===");
        assert!(node1.dht().successors().list()?.contains(&node2.did()));
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert!(node3.dht().successors().list()?.contains(&node2.did()));

        println!("==========================================");
        println!("||  now we start 5 times stabilization  ||");
        println!("==========================================");

        for _ in 0..5 {
            run_stabilization_once(node1.swarm.clone()).await?;
            run_stabilization_once(node2.swarm.clone()).await?;
            run_stabilization_once(node3.swarm.clone()).await?;

            wait_for_msgs([&node1, &node2, &node3]).await;
            assert_no_more_msg([&node1, &node2, &node3]).await;

            println!("=== Check state after stabilization ===");
            assert_eq!(node1.dht().successors().list()?, vec![
                node3.did(),
                node2.did()
            ]);
            assert_eq!(node2.dht().successors().list()?, vec![
                node1.did(),
                node3.did()
            ]);
            assert_eq!(node3.dht().successors().list()?, vec![
                node2.did(),
                node1.did()
            ]);
        }

        println!("=== Check predecessor after all stabilization ===");
        assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
        assert_eq!(*node2.dht().lock_predecessor()?, Some(node3.did()));
        assert_eq!(*node3.dht().lock_predecessor()?, Some(node1.did()));

        Ok(())
    }

    async fn run_stabilization_once(swarm: Arc<Swarm>) -> Result<()> {
        let stab = swarm.stabilizer();
        stab.notify_predecessor().await
    }
}
