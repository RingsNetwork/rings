use async_trait::async_trait;

use crate::dht::ChordStabilize;
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
use crate::swarm::TransportManager;

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

        relay.relay(self.dht.id, None)?;
        self.dht.notify(msg.id)?;
        if let Some(id) = predecessor {
            if id != relay.origin() {
                return self
                    .send_report_message(
                        Message::NotifyPredecessorReport(NotifyPredecessorReport { id }),
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
        if self.swarm.get_and_check_transport(&msg.id).await.is_none()
            && msg.id != self.swarm.address().into()
        {
            self.connect(&msg.id.into()).await?;
        } else {
            {
                self.dht.lock_successor()?.update(msg.id)
            }
            if let Ok(PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
            )) = self.dht.sync_with_successor(msg.id).await
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
    use crate::dht::PeerRing;
    use crate::dht::Stabilization;
    use crate::ecc::SecretKey;
    use crate::message::handlers::connection::test::assert_no_more_msg;
    use crate::message::handlers::connection::test::gen_triple_ordered_keys;
    use crate::message::handlers::connection::test::manually_establish_connection;
    use crate::message::handlers::connection::test::prepare_node;
    use crate::message::handlers::connection::test::test_listen_join_and_init_find_succeesor;
    use crate::message::handlers::connection::test::test_only_two_nodes_establish_connection;
    use crate::swarm::Swarm;

    #[tokio::test]
    async fn test_triple_nodes_stabilization_1_2_3() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_ordered_nodes_stabilization(key1, key2, key3).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_2_3_1() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_ordered_nodes_stabilization(key1, key2, key3).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_3_1_2() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_ordered_nodes_stabilization(key1, key2, key3).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_3_2_1() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_2_1_3() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    #[tokio::test]
    async fn test_triple_nodes_stabilization_1_3_2() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    async fn test_triple_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(&key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(&key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(&key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(
            (&key1, dht1.clone(), &swarm1, &node1),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor((&key3, &node3), (&key2, &node2)).await?;
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

        run_stabilize_once(dht1.clone(), swarm1.clone()).await?;
        run_stabilize_once(dht2.clone(), swarm2.clone()).await?;
        run_stabilize_once(dht3.clone(), swarm3.clone()).await?;

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key2.address());
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did2
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key1.address());
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did1
        ));

        // node2 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key2.address());
        assert_eq!(ev3.relay.path, vec![did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did2
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key3.address());
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did3
        ));

        // node2 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key2.address());
        assert_eq!(ev3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{id}) if id == did1
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
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did1, did2]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did2));
        assert_eq!(*dht2.lock_predecessor()?, Some(did1));
        assert_eq!(*dht3.lock_predecessor()?, Some(did2));

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(dht1.clone(), swarm1.clone()).await?;
        run_stabilize_once(dht2.clone(), swarm2.clone()).await?;
        run_stabilize_once(dht3.clone(), swarm3.clone()).await?;

        // node2 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key2.address());
        assert_eq!(ev3.relay.path, vec![did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did2
        ));

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key2.address());
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did2
        ));

        // node3 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key3.address());
        assert_eq!(ev1.relay.path, vec![did3]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did3
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key1.address());
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did1
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key3.address());
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did3
        ));

        // node1 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key1.address());
        assert_eq!(ev3.relay.path, vec![did3, did1]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{id}) if id == did2
        ));

        // node2 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key2.address());
        assert_eq!(ev3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{id}) if id == did1
        ));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        // node1 -> node2 -> node3
        //   ^                 |
        //   |-----------------|
        // node1's pre is node3, node1's successor is node2
        // node2's pre is node1, node2's successor is node3
        // node3's pre is node2, node3's successor is node1
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
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
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(&key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(&key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(&key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(
            (&key1, dht1.clone(), &swarm1, &node1),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor((&key3, &node3), (&key2, &node2)).await?;
        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node3.listen_once().await.unwrap();
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state before stabilization ===");
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);
        assert!(dht1.lock_predecessor()?.is_none());
        assert!(dht2.lock_predecessor()?.is_none());
        assert!(dht3.lock_predecessor()?.is_none());

        println!("========================================");
        println!("||  now we start first stabilization  ||");
        println!("========================================");

        run_stabilize_once(dht1.clone(), swarm1.clone()).await?;
        run_stabilize_once(dht2.clone(), swarm2.clone()).await?;
        run_stabilize_once(dht3.clone(), swarm3.clone()).await?;

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key2.address());
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did2
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key1.address());
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did1
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key3.address());
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did3
        ));

        // node2 report node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key2.address());
        assert_eq!(ev3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{id}) if id == did1
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
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did2));
        assert_eq!(*dht2.lock_predecessor()?, Some(did3));
        assert!(dht3.lock_predecessor()?.is_none());

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(dht1.clone(), swarm1.clone()).await?;
        run_stabilize_once(dht2.clone(), swarm2.clone()).await?;
        run_stabilize_once(dht3.clone(), swarm3.clone()).await?;

        // node2 notify node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key2.address());
        assert_eq!(ev1.relay.path, vec![did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did2
        ));

        // node1 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key1.address());
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did1
        ));

        // node3 notify node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key3.address());
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(
            ev2.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did3
        ));

        // node2 report node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key2.address());
        assert_eq!(ev1.relay.path, vec![did1, did2]);
        assert!(matches!(
            ev1.data,
            Message::NotifyPredecessorReport(NotifyPredecessorReport{id}) if id == did3
        ));

        // node1 notify node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key1.address());
        assert_eq!(ev3.relay.path, vec![did1]);
        assert!(matches!(
            ev3.data,
            Message::NotifyPredecessorSend(NotifyPredecessorSend{id}) if id == did1
        ));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        // node3 -> node2 -> node1
        //   ^                 |
        //   |-----------------|
        // node1's pre is node2, node1's successor is node3
        // node2's pre is node3, node2's successor is node1
        // node3's pre is none1, node3's successor is node2
        assert_eq!(dht1.lock_successor()?.list(), vec![did3, did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);
        assert_eq!(*dht1.lock_predecessor()?, Some(did2));
        assert_eq!(*dht2.lock_predecessor()?, Some(did3));
        assert_eq!(*dht3.lock_predecessor()?, Some(did1));
        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok(())
    }

    async fn run_stabilize_once(dht: Arc<PeerRing>, swarm: Arc<Swarm>) -> Result<()> {
        Stabilization::new(dht, swarm, 5).stabilize().await
    }
}
