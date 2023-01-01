use async_trait::async_trait;

use crate::dht::Chord;
use crate::dht::ChordStorage;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Result;
use crate::message::types::Message;
use crate::message::types::NotifyPredecessorReport;
use crate::message::types::NotifyPredecessorSend;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::transports::manager::TransportManager;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &NotifyPredecessorSend,
    ) -> Result<()> {
        let mut relay = ctx.relay.clone();
        let predecessor = { *self.dht.lock_predecessor()? };

        relay.relay(self.dht.did, None)?;
        self.dht.notify(msg.did)?;
        if let Some(did) = predecessor {
            if did != relay.origin() {
                return self
                    .send_report_message(
                        Message::NotifyPredecessorReport(NotifyPredecessorReport { did }),
                        ctx.tx_id,
                        relay,
                    )
                    .await;
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorReport> for MessageHandler {
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &NotifyPredecessorReport,
    ) -> Result<()> {
        // if successor: predecessor is between (id, successor]
        // then update local successor
        if self.swarm.get_and_check_transport(msg.did).await.is_none()
            && msg.did != self.swarm.did()
        {
            self.swarm.connect(msg.did).await?;
        } else {
            {
                self.dht.lock_successor()?.update(msg.did)
            }
            if let Ok(PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
            )) = self.dht.sync_vnode_with_successor(msg.did).await
            {
                self.send_direct_message(
                    Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                    next,
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::dht::Stabilization;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::message::handlers::connection::tests::test_listen_join_and_init_find_succeesor;
    use crate::message::handlers::connection::tests::test_only_two_nodes_establish_connection;
    use crate::message::handlers::tests::assert_no_more_msg;
    use crate::message::handlers::tests::wait_for_msgs;
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node;
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

        test_triple_ordered_nodes_stabilization(key1, key2, key3).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_3_1_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_stabilization(key1, key2, key3).await
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
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_1_3_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    async fn test_triple_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor(&node3, &node2).await?;
        node3.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state before stabilization ===");
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);
        assert!(dht1.lock_predecessor()?.is_none());
        assert!(dht2.lock_predecessor()?.is_none());
        assert!(dht3.lock_predecessor()?.is_none());

        println!("========================================");
        println!("||  now we start first stabilization  ||");
        println!("========================================");

        run_stabilize_once(swarm1.clone()).await?;
        run_stabilize_once(swarm2.clone()).await?;
        run_stabilize_once(swarm3.clone()).await?;

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, did2);
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did2
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did1);
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did1
        ));

        // node2 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did2
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did3);
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did3
        ));

        // node2 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{did}) if did == did1
        ));

        // handle messages of connection after stabilization
        node2.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after first stabilization ===");
        // node1 -> node2 -> node3
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node2
        // node2's pre is node1, node2's successor is node3
        // node3's pre is node2, node3's successor is node1
        assert_eq!(dht1.lock_successor()?.list(), vec![did2, did3]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did1, did2]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did2));
        assert_eq!(*dht2.lock_predecessor()?, Some(did1));
        assert_eq!(*dht3.lock_predecessor()?, Some(did2));

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(swarm1.clone()).await?;
        run_stabilize_once(swarm2.clone()).await?;
        run_stabilize_once(swarm3.clone()).await?;

        // node1 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did1);
        assert_eq!(ev3.relay.path, vec![did1]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did1
        ));

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, did2);
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did2
        ));

        // node3 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, did3);
        assert_eq!(ev1.relay.path, vec![did3]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did3
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did1);
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did1
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did3);
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did3
        ));

        // node2 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did2]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did3
        ));

        // node1 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did1);
        assert_eq!(ev3.relay.path, vec![did3, did1]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{did}) if did == did2
        ));

        // node2 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{did}) if did == did1
        ));

        // node3 report node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, did3);
        assert_eq!(ev1.relay.path, vec![did1, did3]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{did}) if did == did2
        ));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        // node1 -> node2 -> node3
        //   ^                 |
        //   |-----------------|
        // node1's pre is node3, node1's successor is node2
        // node2's pre is node1, node2's successor is node3
        // node3's pre is node2, node3's successor is node1
        assert_eq!(dht1.lock_successor()?.list(), vec![did2, did3]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did1, did2]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did3));
        assert_eq!(*dht2.lock_predecessor()?, Some(did1));
        assert_eq!(*dht3.lock_predecessor()?, Some(did2));
        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok(())
    }

    async fn test_triple_desc_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor(&node3, &node2).await?;
        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state before stabilization ===");
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1, did3]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);
        assert!(dht1.lock_predecessor()?.is_none());
        assert!(dht2.lock_predecessor()?.is_none());
        assert!(dht3.lock_predecessor()?.is_none());

        println!("========================================");
        println!("||  now we start first stabilization  ||");
        println!("========================================");

        run_stabilize_once(swarm1.clone()).await?;
        run_stabilize_once(swarm2.clone()).await?;
        run_stabilize_once(swarm3.clone()).await?;

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, did2);
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did2
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did1);
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did1
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did3);
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did3
        ));

        // node2 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{did}) if did == did2
        ));

        // node2 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{did}) if did == did1
        ));

        // handle messages of connection after stabilization
        node2.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        node1.listen_once().await.unwrap();

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after first stabilization ===");
        // node3 -> node2 -> node1
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node2
        // node2's pre is node3, node2's successor is node1
        assert_eq!(dht1.lock_successor()?.list(), vec![did3, did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1, did3]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2, did1]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did2));
        assert_eq!(*dht2.lock_predecessor()?, Some(did3));
        assert_eq!(*dht3.lock_predecessor()?, Some(did2));

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(swarm1.clone()).await?;
        run_stabilize_once(swarm2.clone()).await?;
        run_stabilize_once(swarm3.clone()).await?;

        // TODO: should impl assert_receive by callback like elixir here

        wait_for_msgs(&node1, &node2, &node3).await;
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        // node3 -> node2 -> node1
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node3
        // node2's pre is node3, node2's successor is node1
        // node3's pre is none1, node3's successor is node2
        assert_eq!(dht1.lock_successor()?.list(), vec![did3, did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1, did3]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2, did1]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did2));
        assert_eq!(*dht2.lock_predecessor()?, Some(did3));
        assert_eq!(*dht3.lock_predecessor()?, Some(did1));
        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok(())
    }

    async fn run_stabilize_once(swarm: Arc<Swarm>) -> Result<()> {
        Stabilization::new(swarm, 5).stabilize().await
    }
}
