use std::str::FromStr;

use async_trait::async_trait;

use crate::dht::Chord;
use crate::dht::ChordStorage;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Error;
use crate::err::Result;
use crate::message::types::AlreadyConnected;
use crate::message::types::ConnectNodeReport;
use crate::message::types::ConnectNodeSend;
use crate::message::types::FindSuccessorReport;
use crate::message::types::FindSuccessorSend;
use crate::message::types::JoinDHT;
use crate::message::types::Message;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::HandleMsg;
use crate::message::LeaveDHT;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::prelude::RTCSdpType;
use crate::swarm::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<LeaveDHT> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload<Message>, msg: &LeaveDHT) -> Result<()> {
        let mut dht = self.dht.lock().await;
        dht.remove(msg.id);
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<JoinDHT> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &JoinDHT) -> Result<()> {
        // here is two situation.
        // finger table just have no other node(beside next), it will be a `create` op
        // otherwise, it will be a `send` op
        let mut dht = self.dht.lock().await;
        match dht.join(msg.id) {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(id)) => {
                // if there is only two nodes A, B, it may cause recursion
                // A.successor == B
                // B.successor == A
                // A.find_successor(B)
                if next != ctx.addr.into() {
                    self.send_direct_message(
                        Message::FindSuccessorSend(FindSuccessorSend { id, for_fix: false }),
                        next,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &ConnectNodeSend) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        if dht.id != relay.destination {
            if self.swarm.get_transport(&relay.destination).is_some() {
                relay.relay(dht.id, Some(relay.destination))?;
                return self.transpond_payload(ctx, relay).await;
            } else {
                let next_node = match dht.find_successor(relay.destination)? {
                    PeerRingAction::Some(node) => Some(node),
                    PeerRingAction::RemoteAction(node, _) => Some(node),
                    _ => None,
                }
                .ok_or(Error::MessageHandlerMissNextNode)?;
                relay.relay(dht.id, Some(next_node))?;
                return self.transpond_payload(ctx, relay).await;
            }
        }

        relay.relay(dht.id, None)?;
        match self.swarm.get_transport(&relay.sender()) {
            None => {
                let trans = self.swarm.new_transport().await?;
                let sender_id = relay.sender();
                trans
                    .register_remote_info(msg.handshake_info.to_owned().into())
                    .await?;
                let handshake_info = trans
                    .get_handshake_info(self.swarm.session_manager(), RTCSdpType::Answer)
                    .await?
                    .to_string();
                self.send_report_message(
                    Message::ConnectNodeReport(ConnectNodeReport {
                        transport_uuid: msg.transport_uuid.clone(),
                        handshake_info,
                    }),
                    relay,
                )
                .await?;
                self.swarm.get_or_register(&sender_id, trans).await?;

                Ok(())
            }

            _ => {
                self.send_report_message(Message::AlreadyConnected(AlreadyConnected), relay)
                    .await
            }
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &ConnectNodeReport) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        if relay.next_hop.is_some() {
            self.transpond_payload(ctx, relay).await
        } else {
            let transport = self
                .swarm
                .find_pending_transport(
                    uuid::Uuid::from_str(&msg.transport_uuid)
                        .map_err(|_| Error::InvalidTransportUuid)?,
                )?
                .ok_or(Error::MessageHandlerMissTransportConnectedNode)?;
            transport
                .register_remote_info(msg.handshake_info.clone().into())
                .await?;
            self.swarm.register(&relay.sender(), transport).await
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<AlreadyConnected> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, _msg: &AlreadyConnected) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        if relay.next_hop.is_some() {
            self.transpond_payload(ctx, relay).await
        } else {
            self.swarm
                .get_transport(&relay.sender())
                .map(|_| ())
                .ok_or(Error::MessageHandlerMissTransportAlreadyConnected)
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &FindSuccessorSend) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        match dht.find_successor(msg.id)? {
            PeerRingAction::Some(id) => {
                relay.relay(dht.id, None)?;
                self.send_report_message(
                    Message::FindSuccessorReport(FindSuccessorReport {
                        id,
                        for_fix: msg.for_fix,
                    }),
                    relay,
                )
                .await
            }
            PeerRingAction::RemoteAction(next, _) => {
                relay.relay(dht.id, Some(next))?;
                relay.reset_destination(next)?;
                self.transpond_payload(ctx, relay).await
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &FindSuccessorReport) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        if relay.next_hop.is_some() {
            self.transpond_payload(ctx, relay).await
        } else {
            if self.swarm.get_transport(&msg.id).is_none() && msg.id != self.swarm.address().into()
            {
                self.connect(&msg.id.into()).await?;
                return Ok(());
            }
            if msg.for_fix {
                let fix_finger_index = dht.fix_finger_index;
                dht.finger.set(fix_finger_index as usize, &msg.id);
            } else {
                dht.successor.update(msg.id);
                if let Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
                )) = dht.sync_with_successor(msg.id).await
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
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod test {
    use std::matches;
    use std::sync::Arc;

    use futures::lock::Mutex;
    use tokio::time::sleep;
    use tokio::time::Duration;
    use web3::types::Address;

    use super::*;
    use crate::dht::Did;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::message::MessageHandler;
    use crate::prelude::RTCSdpType;
    use crate::session::SessionManager;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTrickleScheme;

    // ndoe1.key < node2.key < node3.key
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
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_ordered_nodes_connection(key1, key2, key3).await
    }

    // The 2_3_1 should have same behavior as 1_2_3 since they are all clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_2_3_1() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_ordered_nodes_connection(key2, key3, key1).await
    }

    // The 3_1_2 should have same behavior as 1_2_3 since they are all clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_3_1_2() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_ordered_nodes_connection(key3, key1, key2).await
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
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_connection(key3, key2, key1).await
    }

    // The 2_1_3 should have same behavior as 3_2_1 since they are all anti-clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_2_1_3() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_connection(key2, key1, key3).await
    }

    // The 1_3_2 should have same behavior as 3_2_1 since they are all anti-clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_1_3_2() -> Result<()> {
        let (key1, key2, key3) = gen_triple_ordered_keys();
        test_triple_desc_ordered_nodes_connection(key1, key3, key2).await
    }

    async fn test_triple_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1) = prepare_node(&key1).await;
        let (did2, dht2, swarm2, node2) = prepare_node(&key2).await;
        let (did3, dht3, swarm3, node3) = prepare_node(&key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(
            (&key1, dht1.clone(), &swarm1, &node1),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;

        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor((&key3, &node3), (&key2, &node2)).await?;

        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did3, did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);

        // 2->3 FindSuccessorReport
        // node2 report node3 as node3's successor to node3
        //
        // node2 think there is no did that is closer to node3 than node3, so it respond node3
        // to understand that you can imagine the layouts on the clock circle:
        //
        // node2 -> node3 -> node1
        //   ^                 |
        //   |-----------------|
        //
        // as you can see, in node2's view, node1 is farther than node3
        // so node2 pick node3 as node3's successor
        //
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key2.address());
        assert_eq!(ev_3.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!dht3.lock().await.successor.list().contains(&did3));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key3.address());
        assert_eq!(ev_2.relay.path, vec![did2, did3]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.lock().await.successor.list().contains(&did2));

        println!("=== Check state before connect via DHT ===");
        assert_transports(swarm1.clone(), vec![key2.address()]);
        assert_transports(swarm2.clone(), vec![key1.address(), key3.address()]);
        assert_transports(swarm3.clone(), vec![key2.address()]);
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did3, did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        test_connect_via_dht_and_init_find_succeesor(
            (&key1, &swarm1, &node1),
            (&key2, &swarm2, &node2),
            (&key3, &swarm3, &node3),
        )
        .await?;

        // The following are other communications after successful connection

        // 3->1->2 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did3, did1]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did3
        ));

        // 3->1 FindSuccessorReport
        // node3 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key3.address());
        assert_eq!(ev_1.relay.path, vec![did1, did3]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!dht1.lock().await.successor.list().contains(&did1));

        // 2->1 FindSuccessorReport
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key2.address());
        assert_eq!(ev_1.relay.path, vec![did3, did1, did2]);

        // 2->1->3 FindSuccessorReport
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key1.address());
        assert_eq!(ev_3.relay.path, vec![did3, did1, did2]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!dht3.lock().await.successor.list().contains(&did3));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(swarm1, vec![key2.address(), key3.address()]);
        assert_transports(swarm2, vec![key1.address(), key3.address()]);
        assert_transports(swarm3, vec![key1.address(), key2.address()]);
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did3, did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did1, did2]);

        Ok(())
    }

    async fn test_triple_desc_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        let (did1, dht1, swarm1, node1) = prepare_node(&key1).await;
        let (did2, dht2, swarm2, node2) = prepare_node(&key2).await;
        let (did3, dht3, swarm3, node3) = prepare_node(&key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(
            (&key1, dht1.clone(), &swarm1, &node1),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;

        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor((&key3, &node3), (&key2, &node2)).await?;

        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);

        // 3->2->1 FindSuccessorSend
        // node2 think node1 is closer than itself to node3, so it relay msg to node1
        //
        // to understand that you can imagine the layouts on the clock circle:
        //
        // node2 -> node1 -> node3
        //   ^                 |
        //   |-----------------|
        //
        // as you can see, in node2's view, node1 is closer than node2 to node3.
        // so node2 pick node1 to find_successor.
        //
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key2.address());
        assert_eq!(ev_1.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did3
        ));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key3.address());
        assert_eq!(ev_2.relay.path, vec![did2, did3]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.lock().await.successor.list().contains(&did2));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node3's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did3, did2, did1]);
        assert_eq!(ev_2.relay.path_end_cursor, 0);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did2
        ));

        // 1->2->3 FindSuccessorReport
        // node2 relay report to node3
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key2.address());
        assert_eq!(ev_3.relay.path, vec![did3, did2, did1]);
        assert_eq!(ev_3.relay.path_end_cursor, 1);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did2
        ));

        println!("=== Check state before connect via DHT ===");
        assert_transports(swarm1.clone(), vec![key2.address()]);
        assert_transports(swarm2.clone(), vec![key1.address(), key3.address()]);
        assert_transports(swarm3.clone(), vec![key2.address()]);
        assert_eq!(dht1.lock().await.successor.list(), vec![did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        test_connect_via_dht_and_init_find_succeesor(
            (&key1, &swarm1, &node1),
            (&key2, &swarm2, &node2),
            (&key3, &swarm3, &node3),
        )
        .await?;

        // The following are other communications after successful connection

        // 1->3->2 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key3.address());
        assert_eq!(ev_2.relay.path, vec![did1, did3]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did1
        ));

        // 1->3 FindSuccessorReport
        // node1 report node3 as node3's successor to node1
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key1.address());
        assert_eq!(ev_3.relay.path, vec![did3, did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!node3.dht.lock().await.successor.list().contains(&did3));

        // 2->3 FindSuccessorReport
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key2.address());
        assert_eq!(ev_3.relay.path, vec![did1, did3, did2]);

        // 2->3->1 FindSuccessorReport
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key3.address());
        assert_eq!(ev_1.relay.path, vec![did1, did3, did2]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!node1.dht.lock().await.successor.list().contains(&did1));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(swarm1, vec![key2.address(), key3.address()]);
        assert_transports(swarm2, vec![key1.address(), key3.address()]);
        assert_transports(swarm3, vec![key1.address(), key2.address()]);
        assert_eq!(dht1.lock().await.successor.list(), vec![did3, did2]);
        assert_eq!(dht2.lock().await.successor.list(), vec![did1]);
        assert_eq!(dht3.lock().await.successor.list(), vec![did2]);

        Ok(())
    }

    pub fn gen_triple_ordered_keys() -> (SecretKey, SecretKey, SecretKey) {
        let mut keys = Vec::from_iter(std::iter::repeat_with(SecretKey::random).take(3));
        keys.sort_by(|a, b| {
            if a.address() < b.address() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        (keys[0], keys[1], keys[2])
    }

    pub async fn prepare_node(
        key: &SecretKey,
    ) -> (Did, Arc<Mutex<PeerRing>>, Arc<Swarm>, MessageHandler) {
        let stun = "stun://stun.l.google.com:19302";

        let did = key.address().into();
        let dht = Arc::new(Mutex::new(PeerRing::new(did).await.unwrap()));
        let sm = SessionManager::new_with_seckey(key).unwrap();
        let swarm = Arc::new(Swarm::new(stun, key.address(), sm));
        let node = MessageHandler::new(dht.clone(), Arc::clone(&swarm));

        println!("key: {:?}", key.to_string());
        println!("did: {:?}", did);

        (did, dht, swarm, node)
    }

    pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) -> Result<()> {
        let sm1 = swarm1.session_manager();
        let sm2 = swarm2.session_manager();

        let transport1 = swarm1.new_transport().await.unwrap();
        let handshake_info1 = transport1
            .get_handshake_info(sm1, RTCSdpType::Offer)
            .await?;

        let transport2 = swarm2.new_transport().await.unwrap();
        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        assert_eq!(addr1, swarm1.address());

        let handshake_info2 = transport2
            .get_handshake_info(sm2, RTCSdpType::Answer)
            .await?;

        let addr2 = transport1.register_remote_info(handshake_info2).await?;

        assert_eq!(addr2, swarm2.address());

        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;

        swarm2
            .register(&swarm1.address(), transport2.clone())
            .await
            .unwrap();

        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await
            .unwrap();

        assert!(swarm1.get_transport(&swarm2.address()).is_some());
        assert!(swarm2.get_transport(&swarm1.address()).is_some());

        Ok(())
    }

    pub async fn test_listen_join_and_init_find_succeesor(
        (key1, node1): (&SecretKey, &MessageHandler),
        (key2, node2): (&SecretKey, &MessageHandler),
    ) -> Result<()> {
        let did1 = key1.address().into();
        let did2 = key2.address().into();

        // 1 JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key1.address());
        assert_eq!(ev_1.relay.path, vec![did1]);
        assert!(matches!(ev_1.data, Message::JoinDHT(JoinDHT{id}) if id == did2));

        // 2 JoinDHT
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key2.address());
        assert_eq!(ev_2.relay.path, vec![did2]);
        assert!(matches!(ev_2.data, Message::JoinDHT(JoinDHT{id}) if id == did1));

        // 1->2 FindSuccessorSend
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key2.address());
        assert_eq!(ev_1.relay.path, vec![did2]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did2
        ));

        // 2->1 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did1]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did1
        ));

        Ok(())
    }

    pub async fn test_only_two_nodes_establish_connection(
        (key1, dht1, swarm1, node1): (&SecretKey, Arc<Mutex<PeerRing>>, &Swarm, &MessageHandler),
        (key2, dht2, swarm2, node2): (&SecretKey, Arc<Mutex<PeerRing>>, &Swarm, &MessageHandler),
    ) -> Result<()> {
        let did1 = key1.address().into();
        let did2 = key2.address().into();

        manually_establish_connection(swarm1, swarm2).await?;
        test_listen_join_and_init_find_succeesor((key1, node1), (key2, node2)).await?;

        // 2->1 FindSuccessorReport
        // node2 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key2.address());
        assert_eq!(ev_1.relay.path, vec![did1, did2]);
        // node2 is only aware of node1, so it respond node1
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!dht1.lock().await.successor.list().contains(&did1));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did2, did1]);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, for_fix: false}) if id == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.lock().await.successor.list().contains(&did2));

        Ok(())
    }

    async fn test_connect_via_dht_and_init_find_succeesor(
        (key1, swarm1, node1): (&SecretKey, &Swarm, &MessageHandler),
        (key2, _swarm2, node2): (&SecretKey, &Swarm, &MessageHandler),
        (key3, _swarm3, node3): (&SecretKey, &Swarm, &MessageHandler),
    ) -> Result<()> {
        let did1 = key1.address().into();
        let did2 = key2.address().into();
        let did3 = key3.address().into();

        // check node1 and node3 is not connected to each other
        assert!(swarm1.get_transport(&key3.address()).is_none());

        // node1's successor should be node2 now
        assert_eq!(node1.dht.lock().await.successor.max(), did2);

        node1.connect(&key3.address()).await.unwrap();

        // node1 send msg to node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key1.address());
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(ev2.data, Message::ConnectNodeSend(_)));

        // node2 relay msg to node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, key2.address());
        assert_eq!(ev3.relay.path, vec![did1, did2]);
        assert!(matches!(ev3.data, Message::ConnectNodeSend(_)));

        // node3 send report to node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key3.address());
        assert_eq!(ev2.relay.path, vec![did1, did2, did3]);
        assert!(matches!(ev2.data, Message::ConnectNodeReport(_)));

        // node 2 relay report to node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, key2.address());
        assert_eq!(ev1.relay.path, vec![did1, did2, did3]);
        assert!(matches!(ev1.data, Message::ConnectNodeReport(_)));

        assert!(swarm1.get_transport(&key3.address()).is_some());

        // The following are communications after successful connection

        // 1 JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key1.address());
        assert_eq!(ev_1.relay.path, vec![did1]);
        assert!(matches!(ev_1.data, Message::JoinDHT(JoinDHT{id}) if id == did3));

        // 3 JoinDHT
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key3.address());
        assert_eq!(ev_3.relay.path, vec![did3]);
        assert!(matches!(ev_3.data, Message::JoinDHT(JoinDHT{id}) if id == did1));

        // 3->1 FindSuccessorSend
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key3.address());
        assert_eq!(ev_1.relay.path, vec![did3]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did3
        ));

        // 1->3 FindSuccessorSend
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key1.address());
        assert_eq!(ev_3.relay.path, vec![did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, for_fix: false}) if id == did1
        ));

        Ok(())
    }

    pub async fn assert_no_more_msg(
        node1: &MessageHandler,
        node2: &MessageHandler,
        node3: &MessageHandler,
    ) {
        tokio::select! {
            _ = node1.listen_once() => unreachable!(),
            _ = node2.listen_once() => unreachable!(),
            _ = node3.listen_once() => unreachable!(),
            _ = sleep(Duration::from_secs(3)) => {}
        }
    }

    fn assert_transports(swarm: Arc<Swarm>, addresses: Vec<Address>) {
        println!(
            "Check tranport of {:?}: {:?} for addresses {:?}",
            swarm.address(),
            swarm.get_addresses(),
            addresses
        );
        assert_eq!(swarm.get_transports().len(), addresses.len());
        for addr in addresses {
            assert!(swarm.get_transport(&addr).is_some());
        }
    }
}
