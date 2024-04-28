use async_trait::async_trait;

use crate::dht::types::Chord;
use crate::dht::types::CorrectChord;
use crate::dht::PeerRingAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::ConnectNodeReport;
use crate::message::types::ConnectNodeSend;
use crate::message::types::FindSuccessorReport;
use crate::message::types::FindSuccessorSend;
use crate::message::types::Message;
use crate::message::types::QueryForTopoInfoReport;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::types::Then;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

/// QueryForTopoInfoSend is direct message
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<QueryForTopoInfoSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &QueryForTopoInfoSend) -> Result<()> {
        let info: TopoInfo = TopoInfo::try_from(self.dht.as_ref())?;
        if msg.did == self.dht.did {
            self.transport
                .send_report_message(ctx, Message::QueryForTopoInfoReport(msg.resp(info)))
                .await?
        }
        Ok(())
    }
}

/// Try join received node into DHT after received from TopoInfo.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<QueryForTopoInfoReport> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload, msg: &QueryForTopoInfoReport) -> Result<()> {
        match msg.then {
            <QueryForTopoInfoReport as Then>::Then::SyncSuccessor => {
                for peer in msg.info.successors.iter() {
                    self.join_dht(*peer).await?;
                }
            }
            <QueryForTopoInfoReport as Then>::Then::Stabilization => {
                let ev = self.dht.stabilize(msg.info.clone())?;
                self.handle_dht_events(&ev).await?;
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ConnectNodeSend) -> Result<()> {
        if msg.network_id != self.transport.network_id {
            return Ok(());
        }

        if self.dht.did != ctx.relay.destination {
            self.transport.forward_payload(ctx, None).await
        } else {
            let answer = self
                .transport
                .answer_remote_connection(ctx.relay.origin_sender(), self.inner_callback(), msg)
                .await?;
            self.transport
                .send_report_message(ctx, Message::ConnectNodeReport(answer))
                .await
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ConnectNodeReport) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            self.transport.forward_payload(ctx, None).await
        } else {
            self.transport
                .accept_remote_connection(ctx.relay.origin_sender(), msg)
                .await
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FindSuccessorSend) -> Result<()> {
        match self.dht.find_successor(msg.did)? {
            PeerRingAction::Some(did) => {
                if !msg.strict || self.dht.did == msg.did {
                    match &msg.then {
                        FindSuccessorThen::Report(handler) => {
                            self.transport
                                .send_report_message(
                                    ctx,
                                    Message::FindSuccessorReport(FindSuccessorReport {
                                        did,
                                        handler: handler.clone(),
                                    }),
                                )
                                .await
                        }
                    }
                } else {
                    self.transport.forward_payload(ctx, Some(did)).await
                }
            }
            PeerRingAction::RemoteAction(next, _) => {
                self.transport.reset_destination(ctx, next).await
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FindSuccessorReport) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            return self.transport.forward_payload(ctx, None).await;
        }

        match &msg.handler {
            FindSuccessorReportHandler::FixFingerTable | FindSuccessorReportHandler::Connect => {
                if msg.did != self.dht.did {
                    let offer_msg = self
                        .transport
                        .prepare_connection_offer(msg.did, self.inner_callback())
                        .await?;
                    self.transport
                        .send_message(Message::ConnectNodeSend(offer_msg), msg.did)
                        .await?;
                }
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod tests {
    use std::matches;

    use rings_transport::core::transport::WebrtcConnectionState;
    use tokio::time::sleep;
    use tokio::time::Duration;

    use super::*;
    use crate::dht::successor::SuccessorReader;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::tests::default::assert_no_more_msg;
    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::default::Node;
    use crate::tests::manually_establish_connection;

    // node1.key < node2.key < node3.key
    //
    // Firstly, we connect node1 to node2, node2 to node3.
    // Then, we connect node1 to node3 via DHT.
    //
    // After full connected, the topological structure should be:
    //
    // Node1 ------------ Node2 ------------ Node3
    //   |-------------------------------------|
    //
    // --------- Connect node1 and node2
    // 0. Node1 and node2 will set each other as their successor in DHTJoin handler.
    //
    // 1. Node1 send FindSuccessorSend(node1) to node2.
    //    Meanwhile, node2 send FindSuccessorSend(node2) to node1.
    //
    // 2. Node1 respond by sending FindSuccessorReport(node2) to node2.
    //    Meanwhile, node2 respond by sending FindSuccessorReport(node1) to node1.
    //    But no node should update local successor by those reports.
    //
    // --------- Join node3 to node2
    // 0. Node2 and node3 will set each other as their successor in DHTJoin handler.
    //
    // 1. Node3 send FindSuccessorSend(node3) to node2.
    //    Meanwhile, node2 send FindSuccessorSend(node2) to node3.
    //
    // 2. Node3 respond by sending FindSuccessorReport(node2) to node2.
    //    Meanwhile, node2 respond by sending FindSuccessorReport(node3) to node3.
    //    But no node should update local successor by those reports.
    //
    // --------- Connect node1 to node3 via DHT
    // 0. After checking finger table locally, node1 pick node2 to send ConnectNodeSend(node3).
    //
    // 1. Node2 relay ConnectNodeSend(node3) to node3.
    //
    // 2. Node3 respond by sending ConnectNodeReport(node1) to node2.
    //
    // 3. Node2 relay ConnectNodeReport(node1) to node1.
    //
    // --------- Communications after successful connection
    //
    #[tokio::test]
    async fn test_triple_nodes_connection_1_2_3() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_connection(key1, key2, key3).await?;
        Ok(())
    }

    // The 2_3_1 should have same behavior as 1_2_3 since they are all clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_2_3_1() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_connection(key2, key3, key1).await?;
        Ok(())
    }

    // The 3_1_2 should have same behavior as 1_2_3 since they are all clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_3_1_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_connection(key3, key1, key2).await?;
        Ok(())
    }

    // node1.key > node2.key > node3.key
    //
    // All the processes are the same as test_triple_nodes_1_2_3. Except the following:
    //
    // --------- Join node3 to node2
    // 0. Node3 will set node2 as successor in DHTJoin handler.
    //
    //    Node2 will not set node3 as successor in DHTJoin handler.
    //    Because node2.processor.max() is node1, and node1.bias(node1) < node1.bias(node3).
    //    That means node1 is closer to node2 than node3 on the clock circle.
    //
    // 1. Node3 send FindSuccessorSend(node3) to node2. Node2 relay it to Node1.
    //    Meanwhile, node2 send FindSuccessorSend(node2) to node3.
    //
    // 2. Node3 respond by sending FindSuccessorReport(node2) to node2.
    //    Meanwhile, node1 respond by sending FindSuccessorReport(node2) to node3 through node2.
    //
    // --------- Communications after successful connection
    //
    #[tokio::test]
    async fn test_triple_nodes_connection_3_2_1() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_connection(key3, key2, key1).await?;
        Ok(())
    }

    // The 2_1_3 should have same behavior as 3_2_1 since they are all anti-clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_2_1_3() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_connection(key2, key1, key3).await?;
        Ok(())
    }

    // The 1_3_2 should have same behavior as 3_2_1 since they are all anti-clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_1_3_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_desc_ordered_nodes_connection(key1, key3, key2).await?;
        Ok(())
    }

    async fn test_triple_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<(Node, Node, Node)> {
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;
        let node3 = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        node1.assert_transports(vec![node2.did()]);
        node2.assert_transports(vec![node1.did()]);
        node3.assert_transports(vec![]);
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![node1.did()]);
        assert_eq!(node3.dht().successors().list()?, vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state before connect via DHT ===");
        node1.assert_transports(vec![node2.did()]);
        node2.assert_transports(vec![node1.did(), node3.did()]);
        node3.assert_transports(vec![node2.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![node2.did(),]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node1.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        // check node1 and node3 is not connected to each other
        assert!(node1.swarm.transport.get_connection(node3.did()).is_none());
        // node1's successor should be node2 now
        assert_eq!(node1.dht().successors().max()?, node2.did());

        node1.swarm.connect(node3.did()).await.unwrap();
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state after connect via DHT ===");
        node1.assert_transports(vec![node2.did(), node3.did()]);
        node2.assert_transports(vec![node1.did(), node3.did()]);
        node3.assert_transports(vec![node1.did(), node2.did()]);
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

        Ok((node1, node2, node3))
    }

    async fn test_triple_desc_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<(Node, Node, Node)> {
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;
        let node3 = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![node1.did()]);
        assert_eq!(node3.dht().successors().list()?, vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state before connect via DHT ===");
        node1.assert_transports(vec![node2.did()]);
        node2.assert_transports(vec![node1.did(), node3.did()]);
        node3.assert_transports(vec![node2.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        // check node1 and node3 is not connected to each other
        assert!(node1.swarm.transport.get_connection(node3.did()).is_none());
        // node1's successor should be node2 now
        assert_eq!(node1.dht().successors().max()?, node2.did());

        node1.swarm.connect(node3.did()).await.unwrap();
        wait_for_msgs([&node1, &node2, &node3]).await;
        assert_no_more_msg([&node1, &node2, &node3]).await;

        println!("=== Check state after connect via DHT ===");
        node1.assert_transports(vec![node2.did(), node3.did()]);
        node2.assert_transports(vec![node1.did(), node3.did()]);
        node3.assert_transports(vec![node1.did(), node2.did()]);
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

        Ok((node1, node2, node3))
    }

    #[tokio::test]
    async fn test_fourth_node_connection() -> Result<()> {
        let keys = gen_ordered_keys(4);
        let (key1, key2, key3, key4) = (keys[0], keys[1], keys[2], keys[3]);
        let (node1, node2, node3) = test_triple_ordered_nodes_connection(key1, key2, key3).await?;
        // we now have three connected nodes
        // node1 -> node2 -> node3
        //  |-<-----<---------<--|

        let node4 = prepare_node(key4).await;

        // Unless we use a fixed did value, we cannot fully predict the communication order between node4 and the nodes,
        // because we do not know the distance between node4 and each node.
        //
        // Therefore, here we only guarantee that messages can be processed correctly without checking the specific message order.
        //
        // In addition, we check the final state to ensure the entire process meets expectations.

        // connect node4 to node2
        manually_establish_connection(&node4.swarm, &node2.swarm).await;
        tokio::time::sleep(Duration::from_secs(6)).await;

        println!("=== Check state before connect via DHT ===");
        node1.assert_transports(vec![node2.did(), node3.did(), node4.did()]);
        node2.assert_transports(vec![node3.did(), node4.did(), node1.did()]);
        node3.assert_transports(vec![node1.did(), node2.did()]);
        // node4 will connect node1 after connecting node2, because node2 notified node4 that node1 is its predecessor.
        node4.assert_transports(vec![node1.did(), node2.did()]);
        assert_eq!(node1.dht().successors().list().unwrap(), vec![
            node2.did(),
            node3.did(),
            node4.did(),
        ]);
        assert_eq!(node2.dht().successors().list().unwrap(), vec![
            node3.did(),
            node4.did(),
            node1.did(),
        ]);
        assert_eq!(node3.dht().successors().list().unwrap(), vec![
            node1.did(),
            node2.did(),
        ]);
        assert_eq!(node4.dht().successors().list().unwrap(), vec![
            node1.did(),
            node2.did(),
        ]);

        println!("========================================");
        println!("| test node4 connect node3 via dht     |");
        println!("========================================");
        println!(
            "node1.did(): {:?}, node2.did(): {:?}, node3.did(): {:?}, node4.did(): {:?}",
            node1.did(),
            node2.did(),
            node3.did(),
            node4.did(),
        );
        println!("==================================================");

        node4.swarm.connect(node3.did()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(6)).await;

        println!("=== Check state after connect via DHT ===");
        node1.assert_transports(vec![node2.did(), node3.did(), node4.did()]);
        node2.assert_transports(vec![node3.did(), node4.did(), node1.did()]);
        node3.assert_transports(vec![node4.did(), node1.did(), node2.did()]);
        node4.assert_transports(vec![node1.did(), node2.did(), node3.did()]);
        assert_eq!(node1.dht().successors().list().unwrap(), vec![
            node2.did(),
            node3.did(),
            node4.did()
        ]);
        assert_eq!(node2.dht().successors().list().unwrap(), vec![
            node3.did(),
            node4.did(),
            node1.did(),
        ]);
        assert_eq!(node3.dht().successors().list().unwrap(), vec![
            node4.did(),
            node1.did(),
            node2.did(),
        ]);
        assert_eq!(node4.dht().successors().list().unwrap(), vec![
            node1.did(),
            node2.did(),
            node3.did(),
        ]);

        Ok(())
    }

    #[tokio::test]
    async fn test_finger_when_disconnect() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;

        {
            assert!(node1.dht().lock_finger()?.is_empty());
            assert!(node1.dht().lock_finger()?.is_empty());
        }

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        node1.assert_transports(vec![node2.did()]);
        node2.assert_transports(vec![node1.did()]);
        {
            let finger1 = node1.dht().lock_finger()?.clone().clone_finger();
            let finger2 = node2.dht().lock_finger()?.clone().clone_finger();

            assert!(finger1.into_iter().any(|x| x == Some(node2.did())));
            assert!(finger2.into_iter().any(|x| x == Some(node1.did())));
        }

        println!("===================================");
        println!("| test disconnect node1 and node2 |");
        println!("===================================");
        node1.swarm.disconnect(node2.did()).await?;

        for _ in 1..10 {
            println!("wait 3 seconds for node2's transport 2to1 closing");
            sleep(Duration::from_secs(3)).await;
            if let Some(t) = node2.swarm.transport.get_connection(node1.did()) {
                if matches!(
                    t.webrtc_connection_state(),
                    WebrtcConnectionState::Disconnected | WebrtcConnectionState::Closed
                ) {
                    println!("transport 2to1 is disconnected!!!!");
                    break;
                }
            } else {
                println!("transport 2to1 is disappeared!!!!");
                break;
            }
        }

        assert_no_more_msg([&node1, &node2]).await;

        node1.assert_transports(vec![]);
        node2.assert_transports(vec![]);
        {
            let finger1 = node1.dht().lock_finger()?.clone().clone_finger();
            let finger2 = node2.dht().lock_finger()?.clone().clone_finger();
            assert!(finger1.into_iter().all(|x| x.is_none()));
            assert!(finger2.into_iter().all(|x| x.is_none()));
        }

        Ok(())
    }
}
