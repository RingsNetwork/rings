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
use crate::message::RelayMethod;
use crate::swarm::TransportManager;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &NotifyPredecessorSend,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        dht.notify(msg.id);
        if let Some(id) = dht.predecessor {
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
        ctx: &MessagePayload<Message>,
        msg: &NotifyPredecessorReport,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        assert_eq!(relay.method, RelayMethod::REPORT);
        // if successor: predecessor is between (id, successor]
        // then update local successor
        if self.swarm.get_transport(&msg.id).is_none() && msg.id != self.swarm.address().into() {
            self.connect(&msg.id.into()).await?;
            return Ok(());
        }
        dht.successor.update(msg.id);
        if let Ok(PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
        )) = dht.sync_with_successor(msg.id)
        {
            self.send_direct_message(
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

    use futures::lock::Mutex;

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
    async fn test_triple_nodes_stabilization_3_2_1() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_stabilization(key3, key2, key1).await
    }

    async fn test_triple_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1) = prepare_node(&key1);
        let (did2, dht2, swarm2, node2) = prepare_node(&key2);
        let (did3, dht3, swarm3, node3) = prepare_node(&key3);

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
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did3, did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);
        assert!(dht1.lock().await.predecessor.is_none());
        assert!(dht2.lock().await.predecessor.is_none());
        assert!(dht3.lock().await.predecessor.is_none());

        Ok(())
    }

    async fn test_triple_desc_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1) = prepare_node(&key1);
        let (did2, dht2, swarm2, node2) = prepare_node(&key2);
        let (_did3, dht3, swarm3, node3) = prepare_node(&key3);

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
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);
        assert!(dht1.lock().await.predecessor.is_none());
        assert!(dht2.lock().await.predecessor.is_none());
        assert!(dht3.lock().await.predecessor.is_none());

        println!("========================================");
        println!("||  now we start first stabilization  ||");
        println!("========================================");

        run_stabilize_once(dht1.clone(), swarm1.clone()).await?;
        run_stabilize_once(dht2.clone(), swarm2.clone()).await?;
        run_stabilize_once(dht3.clone(), swarm3.clone()).await?;

        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after first stabilization ===");
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht1.lock().await.predecessor, Some(did2));
        assert_eq!(dht2.lock().await.predecessor, Some(did1));
        assert!(dht3.lock().await.predecessor.is_none());

        println!("=========================================");
        println!("||  now we start second stabilization  ||");
        println!("=========================================");

        run_stabilize_once(dht1.clone(), swarm1.clone()).await?;
        run_stabilize_once(dht2.clone(), swarm2.clone()).await?;
        run_stabilize_once(dht3.clone(), swarm3.clone()).await?;

        node1.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        node2.listen_once().await.unwrap();
        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after second stabilization ===");
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht1.lock().await.predecessor, Some(did2));
        assert_eq!(dht2.lock().await.predecessor, Some(did1));
        assert!(dht3.lock().await.predecessor.is_none());

        Ok(())
    }

    async fn run_stabilize_once(dht: Arc<Mutex<PeerRing>>, swarm: Arc<Swarm>) -> Result<()> {
        Stabilization::new(dht, swarm, 5).stabilize().await
    }
}
