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
use crate::message::FindSuccessorThen;
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
        self.disconnect(msg.id.into()).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<JoinDHT> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &JoinDHT) -> Result<()> {
        // here is two situation.
        // finger table just have no other node(beside next), it will be a `create` op
        // otherwise, it will be a `send` op
        match self.dht.join(msg.id)? {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(id)) => {
                // if there is only two nodes A, B, it may cause recursion
                // A.successor == B
                // B.successor == A
                // A.find_successor(B)
                if next != ctx.addr.into() {
                    self.send_direct_message(
                        Message::FindSuccessorSend(FindSuccessorSend {
                            id,
                            then: FindSuccessorThen::Connect,
                        }),
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
        let mut relay = ctx.relay.clone();
        // if id is not dest
        if self.dht.id != relay.destination {
            if self
                .swarm
                .get_and_check_transport(&relay.destination)
                .await
                .is_some()
            {
                relay.relay(self.dht.id, Some(relay.destination))?;
                return self.transpond_payload(ctx, relay).await;
            } else {
                let next_node = match self.dht.find_successor(relay.destination)? {
                    PeerRingAction::Some(node) => Some(node),
                    PeerRingAction::RemoteAction(node, _) => Some(node),
                    _ => None,
                }
                .ok_or(Error::MessageHandlerMissNextNode)?;
                relay.relay(self.dht.id, Some(next_node))?;
                return self.transpond_payload(ctx, relay).await;
            }
        } else {
            // self is dest
            relay.relay(self.dht.id, None)?;
            match self.swarm.get_and_check_transport(&relay.sender()).await {
                None => {
                    let trans = self.swarm.new_transport().await?;
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
                        ctx.tx_id,
                        relay,
                    )
                    .await?;
                    self.swarm.push_pending_transport(&trans)?;
                    Ok(())
                }

                _ => {
                    self.send_report_message(
                        Message::AlreadyConnected(AlreadyConnected),
                        ctx.tx_id,
                        relay,
                    )
                    .await
                }
            }
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &ConnectNodeReport) -> Result<()> {
        let mut relay = ctx.relay.clone();

        relay.relay(self.dht.id, None)?;
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
            Ok(())
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<AlreadyConnected> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, _msg: &AlreadyConnected) -> Result<()> {
        let mut relay = ctx.relay.clone();

        relay.relay(self.dht.id, None)?;
        if relay.next_hop.is_some() {
            self.transpond_payload(ctx, relay).await
        } else {
            self.swarm
                .get_and_check_transport(&relay.sender())
                .await
                .map(|_| ())
                .ok_or(Error::MessageHandlerMissTransportAlreadyConnected)
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &FindSuccessorSend) -> Result<()> {
        let mut relay = ctx.relay.clone();

        match self.dht.find_successor(msg.id)? {
            PeerRingAction::Some(id) => {
                relay.relay(self.dht.id, None)?;
                self.send_report_message(
                    Message::FindSuccessorReport(FindSuccessorReport { id, then: msg.then }),
                    ctx.tx_id,
                    relay,
                )
                .await
            }
            PeerRingAction::RemoteAction(next, _) => {
                relay.relay(self.dht.id, Some(next))?;
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
        let mut relay = ctx.relay.clone();

        relay.relay(self.dht.id, None)?;
        if relay.next_hop.is_some() {
            return self.transpond_payload(ctx, relay).await;
        }

        match msg.then {
            FindSuccessorThen::FixFingerTable => self.dht.lock_finger()?.set_fix(msg.id),
            FindSuccessorThen::Connect => {
                if self.swarm.get_and_check_transport(&msg.id).await.is_none()
                    && msg.id != self.swarm.address().into()
                {
                    self.connect(&msg.id.into()).await?;
                }
            }
            FindSuccessorThen::SyncStorage => {
                self.dht.lock_successor()?.update(msg.id);
                if let Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
                )) = self.dht.sync_with_successor(msg.id).await
                {
                    self.send_direct_message(
                        Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                        next,
                    )
                    .await?
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
    use std::sync::Arc;

    use tokio::time::sleep;
    use tokio::time::Duration;
    use web3::types::Address;

    use super::*;
    use crate::dht::PeerRing;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::message::handlers::tests::manually_establish_connection;
    use crate::message::handlers::tests::prepare_node;
    use crate::message::MessageHandler;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTransportInterface;

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
    ) -> Result<(MessageHandler, MessageHandler, MessageHandler)> {
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

        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor((&key3, &node3), (&key2, &node2)).await?;

        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);

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
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!dht3.lock_successor()?.list().contains(&did3));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key3.address());
        assert_eq!(ev_2.relay.path, vec![did2, did3]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.lock_successor()?.list().contains(&did2));

        println!("=== Check state before connect via DHT ===");
        assert_transports(swarm1.clone(), vec![key2.address()]);
        assert_transports(swarm2.clone(), vec![key1.address(), key3.address()]);
        assert_transports(swarm3.clone(), vec![key2.address()]);
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);

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
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did3
        ));

        // 3->1 FindSuccessorReport
        // node3 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, key3.address());
        assert_eq!(ev_1.relay.path, vec![did1, did3]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!dht1.lock_successor()?.list().contains(&did1));

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
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!dht3.lock_successor()?.list().contains(&did3));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(swarm1, vec![key2.address(), key3.address()]);
        assert_transports(swarm2, vec![key1.address(), key3.address()]);
        assert_transports(swarm3, vec![key1.address(), key2.address()]);
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did3, did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did1, did2]);
        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok((node1, node2, node3))
    }

    async fn test_triple_desc_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<(MessageHandler, MessageHandler, MessageHandler)> {
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

        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor((&key3, &node3), (&key2, &node2)).await?;

        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);

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
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did3
        ));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key3.address());
        assert_eq!(ev_2.relay.path, vec![did2, did3]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.lock_successor()?.list().contains(&did2));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node3's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did3, did2, did1]);
        assert_eq!(ev_2.relay.path_end_cursor, 0);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did2
        ));

        // 1->2->3 FindSuccessorReport
        // node2 relay report to node3
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key2.address());
        assert_eq!(ev_3.relay.path, vec![did3, did2, did1]);
        assert_eq!(ev_3.relay.path_end_cursor, 1);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did2
        ));

        println!("=== Check state before connect via DHT ===");
        assert_transports(swarm1.clone(), vec![key2.address()]);
        assert_transports(swarm2.clone(), vec![key1.address(), key3.address()]);
        assert_transports(swarm3.clone(), vec![key2.address()]);
        assert_eq!(dht1.lock_successor()?.list(), vec![did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);

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
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did1
        ));

        // 1->3 FindSuccessorReport
        // node1 report node3 as node3's successor to node1
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key1.address());
        assert_eq!(ev_3.relay.path, vec![did3, did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!node3.dht.lock_successor()?.list().contains(&did3));

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
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!node1.dht.lock_successor()?.list().contains(&did1));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(swarm1, vec![key2.address(), key3.address()]);
        assert_transports(swarm2, vec![key1.address(), key3.address()]);
        assert_transports(swarm3, vec![key1.address(), key2.address()]);
        assert_eq!(dht1.lock_successor()?.list(), vec![did3, did2]);
        assert_eq!(dht2.lock_successor()?.list(), vec![did1]);
        assert_eq!(dht3.lock_successor()?.list(), vec![did2]);

        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok((node1, node2, node3))
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
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did2
        ));

        // 2->1 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did1]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did1
        ));

        Ok(())
    }

    pub async fn test_only_two_nodes_establish_connection(
        (key1, dht1, swarm1, node1): (&SecretKey, Arc<PeerRing>, &Swarm, &MessageHandler),
        (key2, dht2, swarm2, node2): (&SecretKey, Arc<PeerRing>, &Swarm, &MessageHandler),
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
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!dht1.lock_successor()?.list().contains(&did1));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, key1.address());
        assert_eq!(ev_2.relay.path, vec![did2, did1]);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{id, then: FindSuccessorThen::Connect}) if id == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.lock_successor()?.list().contains(&did2));

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
        assert_eq!(node1.dht.lock_successor()?.max(), did2);

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

        //        assert!(swarm1.get_transport(&key3.address()).is_some());

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
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did3
        ));

        // 1->3 FindSuccessorSend
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, key1.address());
        assert_eq!(ev_3.relay.path, vec![did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorSend(FindSuccessorSend{id, then: FindSuccessorThen::Connect}) if id == did1
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

    #[tokio::test]
    async fn test_quadra_desc_node_connection() -> Result<()> {
        // 1. node1 to node2
        // 2. node 3 to node 2
        let keys = gen_ordered_keys(4);
        let (key1, key2, key3, key4) = (keys[0], keys[1], keys[2], keys[3]);
        let (node1, node2, node3) = test_triple_ordered_nodes_connection(key1, key2, key3).await?;
        // we now have triple connected node
        let (_, _, swarm4, node4, _path4) = prepare_node(&key4).await;
        // connect node 4 to node2
        manually_establish_connection(&swarm4, &node2.swarm).await?;
        test_listen_join_and_init_find_succeesor((&key4, &node4), (&key2, &node2)).await?;
        // node 1 -> node 2 -> node 3
        //  |-<-----<---------<--|
        let _ = node2.listen_once().await.unwrap();
        let _ = node3.listen_once().await.unwrap();
        let _ = node2.listen_once().await.unwrap();
        let _ = node4.listen_once().await.unwrap();
        let _ = node2.listen_once().await.unwrap();
        println!("==================================================");
        println!("| test connect node 4 from node 1 via node 2     |");
        println!("==================================================");
        println!(
            "did1: {:?}, did2: {:?}, did3: {:?}, did4: {:?}",
            key1.address(),
            key2.address(),
            key3.address(),
            key4.address()
        );
        println!("==================================================");
        node4.connect(&key1.address()).await?;
        // node 4 send msg to node2
        assert!(matches!(
            node2.listen_once().await.unwrap().data,
            Message::ConnectNodeSend(_)
        ));
        // node 2 relay to node 2
        assert!(matches!(
            node1.listen_once().await.unwrap().data,
            Message::ConnectNodeSend(_)
        ));
        // report to node 2
        assert!(matches!(
            node2.listen_once().await.unwrap().data,
            Message::ConnectNodeReport(_)
        ));
        // report to node 4
        assert!(matches!(
            node4.listen_once().await.unwrap().data,
            Message::ConnectNodeReport(_)
        ));
        println!("=================Finish handshake here=================");
        Ok(())
    }

    #[tokio::test]
    async fn test_finger_when_disconnect() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let key3 = SecretKey::random();

        let (did1, dht1, swarm1, node1, _path1) = prepare_node(&key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(&key2).await;

        // This is only a dummy node for using assert_no_more_msg function
        let (_did3, _dht3, _swarm3, node3, _path3) = prepare_node(&key3).await;

        {
            assert!(dht1.lock_finger()?.is_empty());
            assert!(dht1.lock_finger()?.is_empty());
        }

        test_only_two_nodes_establish_connection(
            (&key1, dht1.clone(), &swarm1, &node1),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;
        assert_no_more_msg(&node1, &node2, &node3).await;

        assert_transports(swarm1.clone(), vec![key2.address()]);
        assert_transports(swarm2.clone(), vec![key1.address()]);
        {
            let finger1 = dht1.lock_finger()?.clone().clone_finger();
            let finger2 = dht2.lock_finger()?.clone().clone_finger();

            assert!(finger1.into_iter().any(|x| x == Some(did2)));
            assert!(finger2.into_iter().any(|x| x == Some(did1)));
        }

        println!("===================================");
        println!("| test disconnect node1 and node2 |");
        println!("===================================");
        node1.disconnect(did2.into()).await?;

        // The transport is already dropped by disconnect function.
        // So that we get no msg from this listening.
        let ev1 = node1.listen_once().await;
        assert!(ev1.is_none());

        #[cfg(not(feature = "wasm"))]
        swarm2.get_transport(&did1).unwrap().close().await.unwrap();

        for _ in 1..10 {
            println!("wait 3 seconds for node2's transport 2to1 closing");
            sleep(Duration::from_secs(3)).await;
            if let Some(t) = swarm2.get_transport(&did1) {
                if t.is_disconnected().await {
                    println!("transport 2to1 is disconnected!!!!");
                    break;
                }
            } else {
                println!("transport 2to1 is disappeared!!!!");
                break;
            }
        }
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, key2.address());
        assert!(matches!(ev2.data, Message::LeaveDHT(LeaveDHT{id}) if id == did1));

        assert_no_more_msg(&node1, &node2, &node3).await;

        assert_transports(swarm1.clone(), vec![]);
        assert_transports(swarm2.clone(), vec![]);
        {
            let finger1 = dht1.lock_finger()?.clone().clone_finger();
            let finger2 = dht2.lock_finger()?.clone().clone_finger();
            assert!(finger1.into_iter().all(|x| x.is_none()));
            assert!(finger2.into_iter().all(|x| x.is_none()));
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_already_connect_fixture() -> Result<()> {
        // NodeA-NodeB-NodeC
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(&key1).await;
        let (_did2, dht2, swarm2, node2, _path2) = prepare_node(&key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(&key3).await;
        test_only_two_nodes_establish_connection(
            (&key1, dht1.clone(), &swarm1, &node1),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;
        assert_no_more_msg(&node1, &node2, &node3).await;

        test_only_two_nodes_establish_connection(
            (&key3, dht3.clone(), &swarm3, &node3),
            (&key2, dht2.clone(), &swarm2, &node2),
        )
        .await?;
        assert_no_more_msg(&node1, &node2, &node3).await;
        // Node 1 -- Node 2 -- Node 3
        println!("node1 connect node2 twice here");
        let _ = node1.connect(&did3).await.unwrap();
        let t_1_3_b = node1.connect(&did3).await.unwrap();
        // ConnectNodeSend
        let _ = node2.listen_once().await.unwrap();
        let _ = node2.listen_once().await.unwrap();
        // ConnectNodeSend
        let _ = node3.listen_once().await.unwrap();
        let _ = node3.listen_once().await.unwrap();
        // ConnectNodeReport
        // `self.swarm.push_pending_transport(&trans)?;`
        let _ = node2.listen_once().await.unwrap();
        let _ = node2.listen_once().await.unwrap();
        // ConnectNodeReport
        // self.swarm.register(&relay.sender(), transport).await
        let _ = node1.listen_once().await.unwrap();
        let _ = node1.listen_once().await.unwrap();
        println!("wait for handshake here");
        sleep(Duration::from_secs(3)).await;
        // transport got from node1 for node3
        // transport got from node3 for node
        // JoinDHT twice here
        let ev3 = node3.listen_once().await.unwrap();
        assert!(matches!(ev3.data, Message::JoinDHT(_)));
        let _ = node3.listen_once().await.is_none();

        // JoinDHT twice here
        let ev1 = node1.listen_once().await.unwrap();
        assert!(matches!(ev1.data, Message::JoinDHT(_)));
        let _ = node1.listen_once().await.is_none();

        let t1_3 = swarm1.get_transport(&did3).unwrap();
        println!("transport is replace by second");
        assert_eq!(t1_3.id, t_1_3_b.id);
        let t3_1 = swarm3.get_transport(&did1).unwrap();

        assert!(t1_3.is_connected().await);
        assert!(t3_1.is_connected().await);
        Ok(())
    }
}
