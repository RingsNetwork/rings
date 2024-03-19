use async_trait::async_trait;

use crate::dht::Chord;
use crate::dht::ChordStorageSync;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::NotifyPredecessorReport;
use crate::message::types::NotifyPredecessorSend;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &NotifyPredecessorSend) -> Result<()> {
        let predecessor = self.dht.notify(msg.did)?;

        if predecessor != ctx.relay.origin_sender() {
            return self
                .transport
                .send_report_message(
                    ctx,
                    Message::NotifyPredecessorReport(NotifyPredecessorReport { did: predecessor }),
                )
                .await;
        }

        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorReport> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload, msg: &NotifyPredecessorReport) -> Result<()> {
        self.transport
            .connect(msg.did, self.inner_callback())
            .await?;

        if let Ok(PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
        )) = self.dht.sync_vnode_with_successor(msg.did).await
        {
            self.transport
                .send_message(
                    Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                    next,
                )
                .await?;
        }

        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::dht::successor::SuccessorReader;
    use crate::dht::Stabilization;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::swarm::Swarm;
    use crate::tests::default::assert_no_more_msg;
    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::manually_establish_connection;

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
        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

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

        run_stabilize_once(node1.swarm.clone()).await?;
        run_stabilize_once(node2.swarm.clone()).await?;
        run_stabilize_once(node3.swarm.clone()).await?;

        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after first stabilization ===");
        // node1 -> node2 -> node3
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node2
        // node2's pre is node1, node2's successor is node3
        // node3's pre is node2, node3's successor is node1
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
        assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
        assert_eq!(*node2.dht().lock_predecessor()?, Some(node1.did()));
        assert_eq!(*node3.dht().lock_predecessor()?, Some(node2.did()));

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(node1.swarm.clone()).await?;
        run_stabilize_once(node2.swarm.clone()).await?;
        run_stabilize_once(node3.swarm.clone()).await?;

        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        // node1 -> node2 -> node3
        //   ^                 |
        //   |-----------------|
        // node1's pre is node3, node1's successor is node2
        // node2's pre is node1, node2's successor is node3
        // node3's pre is node2, node3's successor is node1
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
        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

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

        run_stabilize_once(node1.swarm.clone()).await?;
        run_stabilize_once(node2.swarm.clone()).await?;
        run_stabilize_once(node3.swarm.clone()).await?;

        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after first stabilization ===");
        // node3 -> node2 -> node1
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node2
        // node2's pre is node3, node2's successor is node1
        // node3's pre is node2, node3's successor is node2
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
        assert_eq!(*node2.dht().lock_predecessor()?, Some(node3.did()));
        assert_eq!(*node3.dht().lock_predecessor()?, Some(node2.did()));

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(node1.swarm.clone()).await?;
        run_stabilize_once(node2.swarm.clone()).await?;
        run_stabilize_once(node3.swarm.clone()).await?;

        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        // node3 -> node2 -> node1
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node3
        // node2's pre is node3, node2's successor is node1
        // node3's pre is node2, node3's successor is node2
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
        assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
        assert_eq!(*node2.dht().lock_predecessor()?, Some(node3.did()));
        assert_eq!(*node3.dht().lock_predecessor()?, Some(node2.did()));

        println!("=========================================");
        println!("||  now we start third stabilization   ||");
        println!("=========================================");

        run_stabilize_once(node1.swarm.clone()).await?;
        run_stabilize_once(node2.swarm.clone()).await?;
        run_stabilize_once(node3.swarm.clone()).await?;

        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after third stabilization ===");
        // node3 -> node2 -> node1
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node3
        // node2's pre is node3, node2's successor is node1
        // node3's pre is none1, node3's successor is node2
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
        assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
        assert_eq!(*node2.dht().lock_predecessor()?, Some(node3.did()));
        assert_eq!(*node3.dht().lock_predecessor()?, Some(node1.did()));

        Ok(())
    }

    async fn run_stabilize_once(swarm: Arc<Swarm>) -> Result<()> {
        let stab = Stabilization::new(swarm, 5);
        stab.notify_predecessor().await
    }
}
