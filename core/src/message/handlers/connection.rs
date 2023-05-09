use std::ops::Deref;
use std::str::FromStr;

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::dht::Chord;
use crate::dht::ChordStorage;
use crate::dht::CorrectChord;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::TopoInfo;
use crate::err::Error;
use crate::err::Result;
use crate::message::types::AlreadyConnected;
use crate::message::types::ConnectNodeReport;
use crate::message::types::ConnectNodeSend;
use crate::message::types::FindSuccessorReport;
use crate::message::types::FindSuccessorSend;
use crate::message::types::JoinDHT;
use crate::message::types::Message;
use crate::message::types::QueryForTopoInfoReport;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::HandleMsg;
use crate::message::LeaveDHT;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::WrappedDid;
use crate::transports::manager::TransportHandshake;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;

#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub async fn handle_update_successor(
    handler: &MessageHandler,
    act: PeerRingAction,
    ctx: &MessagePayload<Message>,
) -> Result<()> {
    match act {
        PeerRingAction::None => Ok(()),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
            handler
                .send_direct_message(
                    Message::QueryForTopoInfoSend(QueryForTopoInfoSend { did: next }),
                    next,
                )
                .await?;
            Ok(())
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_update_successor(handler, act, ctx).await?;
            }
            Ok(())
        }
        PeerRingAction::RemoteAction(did, PeerRingRemoteAction::TryConnect) => {
            handler.swarm.connect_via(did, ctx.relay.sender()).await?;
            Ok(())
        }
        _ => unreachable!(),
    }
}

#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_join_dht(
    handler: &MessageHandler,
    act: PeerRingAction,
    ctx: &MessagePayload<Message>,
) -> Result<()> {
    match act {
        PeerRingAction::None => Ok(()),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did)) => {
            // if there is only two nodes A, B, it may cause recursion
            // A.successor == B
            // B.successor == A
            // A.find_successor(B)
            if next != ctx.addr {
                handler
                    .send_direct_message(
                        Message::FindSuccessorSend(FindSuccessorSend {
                            did,
                            strict: false,
                            then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                        }),
                        next,
                    )
                    .await?;
            }
            Ok(())
        }
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
            handler
                .send_direct_message(
                    Message::QueryForTopoInfoSend(QueryForTopoInfoSend { did: next }),
                    next,
                )
                .await?;
            Ok(())
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_join_dht(handler, act, ctx).await?;
            }
            Ok(())
        }
        _ => unreachable!(),
    }
}

/// QueryForTopoInfoSend is direct message
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<QueryForTopoInfoSend> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &QueryForTopoInfoSend,
    ) -> Result<()> {
        let info: TopoInfo = TopoInfo::try_from(self.dht.deref())?;
        if msg.did == self.dht.did {
            self.send_report_message(
                ctx,
                Message::QueryForTopoInfoReport(QueryForTopoInfoReport { info }),
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<QueryForTopoInfoReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &QueryForTopoInfoReport,
    ) -> Result<()> {
        let successors: Vec<WrappedDid> = msg
            .info
            .successors
            .iter()
            .map(|did| WrappedDid::new(&self.swarm, did.clone()))
            .collect();
        let act = self.dht.extend_successor(&successors).await?;
        handle_update_successor(self, act, ctx).await?;
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<LeaveDHT> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload<Message>, msg: &LeaveDHT) -> Result<()> {
        self.swarm.disconnect(msg.did).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<JoinDHT> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &JoinDHT) -> Result<()> {
        // here is two situation.
        // finger table just have no other node(beside next), it will be a `create` op
        // otherwise, it will be a `send` op
        #[cfg(feature = "experimental")]
        {
            let w_did = WrappedDid::new(&self.swarm, msg.did);
            let act = self.dht.join_then_sync(w_did).await?;
            handle_join_dht(self, act, ctx).await?;
            Ok(())
        }
        #[cfg(not(feature = "experimental"))]
        {
            let act = self.dht.join(msg.did)?;
            handle_join_dht(self, act, ctx).await
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &ConnectNodeSend) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            if self
                .swarm
                .get_and_check_transport(ctx.relay.destination)
                .await
                .is_some()
            {
                return self.forward_payload(ctx, Some(ctx.relay.destination)).await;
            } else {
                return self.forward_payload(ctx, None).await;
            }
        }

        match self
            .swarm
            .answer_remote_transport(ctx.relay.sender(), msg)
            .await
        {
            Ok((_, msg)) => {
                self.send_report_message(ctx, Message::ConnectNodeReport(msg))
                    .await
            }

            Err(Error::AlreadyConnected) => {
                self.send_report_message(ctx, Message::AlreadyConnected(AlreadyConnected))
                    .await
            }

            Err(e) => Result::Err(e),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &ConnectNodeReport) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            if self
                .swarm
                .get_and_check_transport(ctx.relay.destination)
                .await
                .is_some()
            {
                return self.forward_payload(ctx, Some(ctx.relay.destination)).await;
            } else {
                return self.forward_payload(ctx, None).await;
            }
        }

        let transport = self
            .swarm
            .find_pending_transport(
                uuid::Uuid::from_str(&msg.transport_uuid)
                    .map_err(|_| Error::InvalidTransportUuid)?,
            )?
            .ok_or(Error::MessageHandlerMissTransportConnectedNode)?;
        transport
            .register_remote_info(&msg.answer, ctx.relay.sender())
            .await?;
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<AlreadyConnected> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, _msg: &AlreadyConnected) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            return self.forward_payload(ctx, None).await;
        }
        self.swarm
            .get_and_check_transport(ctx.relay.sender())
            .await
            .map(|_| ())
            .ok_or(Error::MessageHandlerMissTransportAlreadyConnected)
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &FindSuccessorSend) -> Result<()> {
        match self.dht.find_successor(msg.did)? {
            PeerRingAction::Some(did) => {
                if !msg.strict || self.dht.did == msg.did {
                    match &msg.then {
                        FindSuccessorThen::Report(handler) => {
                            self.send_report_message(
                                ctx,
                                Message::FindSuccessorReport(FindSuccessorReport {
                                    did,
                                    handler: handler.clone(),
                                }),
                            )
                            .await
                        }
                    }
                } else if self.swarm.get_and_check_transport(msg.did).await.is_some() {
                    return self.forward_payload(ctx, Some(msg.did)).await;
                } else {
                    return Err(Error::MessageHandlerMissNextNode);
                }
            }
            PeerRingAction::RemoteAction(next, _) => self.reset_destination(ctx, next).await,
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &FindSuccessorReport) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            if self
                .swarm
                .get_and_check_transport(ctx.relay.destination)
                .await
                .is_some()
            {
                return self.forward_payload(ctx, Some(ctx.relay.destination)).await;
            } else {
                return self.forward_payload(ctx, None).await;
            }
        }

        match &msg.handler {
            FindSuccessorReportHandler::FixFingerTable => {
                if self.swarm.get_and_check_transport(msg.did).await.is_none()
                    && msg.did != self.swarm.did()
                {
                    self.swarm.connect(msg.did).await?;
                } else {
                    self.dht.join(msg.did)?;
                }
            }
            FindSuccessorReportHandler::Connect => {
                if self.swarm.get_and_check_transport(msg.did).await.is_none()
                    && msg.did != self.swarm.did()
                {
                    self.swarm.connect(msg.did).await?;
                }
            }
            FindSuccessorReportHandler::SyncStorage => {
                let w_did = WrappedDid::new(&self.swarm, msg.did);
                let updated_act = self.dht.update_successor(w_did).await?;
                handle_update_successor(self, updated_act, ctx).await?;
                if let Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
                )) = self.dht.sync_vnode_with_successor(msg.did).await
                {
                    self.send_message(
                        Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                        next,
                    )
                    .await?;
                    return Ok(());
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

    use super::*;
    use crate::dht::successor::SuccessorReader;
    use crate::dht::Did;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::message::handlers::tests::assert_no_more_msg;
    use crate::message::MessageHandler;
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node;
    use crate::tests::manually_establish_connection;
    use crate::transports::manager::TransportManager;
    use crate::types::ice_transport::IceTransportInterface;
    use crate::tests::TestContext;

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
        test_triple_ordered_nodes_connection(key1, key2, key3, None).await?;
        Ok(())
    }

    // The 2_3_1 should have same behavior as 1_2_3 since they are all clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_2_3_1() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_connection(key2, key3, key1, None).await?;
        Ok(())
    }

    // The 3_1_2 should have same behavior as 1_2_3 since they are all clockwise.
    #[tokio::test]
    async fn test_triple_nodes_connection_3_1_2() -> Result<()> {
        let keys = gen_ordered_keys(3);
        let (key1, key2, key3) = (keys[0], keys[1], keys[2]);
        test_triple_ordered_nodes_connection(key3, key1, key2, None).await?;
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

    async fn check_update_successor(
        node: &MessageHandler,
        expect_successors: Option<&Vec<Did>>,
	ctx: Option<&TestContext<'_>>
    ) -> Result<()> {
        let ev = node.listen_once().await.unwrap();
        if let Message::QueryForTopoInfoReport(rep) = ev.data {
            if let Some(succs) = expect_successors {
                assert!(&rep.info.successors == succs);
            }
            for did in rep.info.successors {
                if node.swarm.get_transport(did).is_none() {
                    println!("TODO: May send connect message here");
                }
            }
        } else {
            panic!("unexpected message");
        }
        Ok(())
    }

    async fn test_triple_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
	ctx: Option<&TestContext<'_>>
    ) -> Result<(MessageHandler, MessageHandler, MessageHandler)> {
        println!("========================================");
        println!("FN test_triple_ordered_nodes_connection BEGIN");
        println!("========================================");

        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(&node1, &node2, ctx).await?;

        assert_eq!(dht1.successors().list()?, vec![did2]);
        assert_eq!(dht2.successors().list()?, vec![did1]);
        assert_eq!(dht3.successors().list()?, vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor(&node3, &node2).await?;

        assert_eq!(dht1.successors().list()?, vec![did2]);
        assert_eq!(dht2.successors().list()?, vec![did3, did1]);
        assert_eq!(dht3.successors().list()?, vec![did2]);

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
        assert_eq!(ev_3.addr, did2);
        assert_eq!(ev_3.relay.path, vec![did2]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!dht3.successors().list()?.contains(&did3));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did3);
        assert_eq!(ev_2.relay.path, vec![did3]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.successors().list()?.contains(&did2));

        println!("=== Check state before connect via DHT ===");
        assert_transports(swarm1.clone(), vec![did2]);
        assert_transports(swarm2.clone(), vec![did1, did3]);
        assert_transports(swarm3.clone(), vec![did2]);
        assert_eq!(dht1.successors().list()?, vec![did2]);
        assert_eq!(dht2.successors().list()?, vec![did3, did1]);
        assert_eq!(dht3.successors().list()?, vec![did2]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        test_connect_via_dht_and_init_find_succeesor(&node1, &node2, &node3).await?;

        // The following are other communications after successful connection

        // 3->1->2 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did1);
        assert_eq!(ev_2.relay.path, vec![did3, did1]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == did3
        ));

        // 3->1 FindSuccessorReport
        // node3 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did3);
        assert_eq!(ev_1.relay.path, vec![did3]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!dht1.successors().list()?.contains(&did1));

        // 2->1 FindSuccessorReport
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did2);
        assert_eq!(ev_1.relay.path, vec![did2]);

        // 2->1->3 FindSuccessorReport
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, did1);
        assert_eq!(ev_3.relay.path, vec![did2, did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!dht3.successors().list()?.contains(&did3));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(swarm1, vec![did2, did3]);
        assert_transports(swarm2, vec![did1, did3]);
        assert_transports(swarm3, vec![did1, did2]);
        assert_eq!(dht1.successors().list()?, vec![did2, did3]);
        assert_eq!(dht2.successors().list()?, vec![did3, did1]);
        assert_eq!(dht3.successors().list()?, vec![did1, did2]);
        tokio::fs::remove_dir_all("./tmp").await.ok();
        println!("========================================");
        println!("FN test_triple_ordered_nodes_connection END");
        println!("========================================");

        Ok((node1, node2, node3))
    }

    async fn test_triple_desc_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<(MessageHandler, MessageHandler, MessageHandler)> {
        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;
        let (did3, dht3, swarm3, node3, _path3) = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(&node1, &node2, None).await?;

        assert_eq!(dht1.successors().list()?, vec![did2]);
        assert_eq!(dht2.successors().list()?, vec![did1]);
        assert_eq!(dht3.successors().list()?, vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&swarm3, &swarm2).await?;
        test_listen_join_and_init_find_succeesor(&node3, &node2).await?;

        assert_eq!(dht1.successors().list()?, vec![did2]);
        assert_eq!(dht2.successors().list()?, vec![did1, did3]);
        assert_eq!(dht3.successors().list()?, vec![did2]);

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
        assert_eq!(ev_1.addr, did2);
        assert_eq!(ev_1.relay.path, vec![did3, did2]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == did3
        ));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did3);
        assert_eq!(ev_2.relay.path, vec![did3]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.successors().list()?.contains(&did2));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node3's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did1);
        assert_eq!(ev_2.relay.path, vec![did1]);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did2
        ));

        // 1->2->3 FindSuccessorReport
        // node2 relay report to node3
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, did2);
        assert_eq!(ev_3.relay.path, vec![did1, did2]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did2
        ));

        println!("=== Check state before connect via DHT ===");
        assert_transports(swarm1.clone(), vec![did2]);
        assert_transports(swarm2.clone(), vec![did1, did3]);
        assert_transports(swarm3.clone(), vec![did2]);
        assert_eq!(dht1.successors().list()?, vec![did2]);
        assert_eq!(dht2.successors().list()?, vec![did1, did3]);
        assert_eq!(dht3.successors().list()?, vec![did2]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        test_connect_via_dht_and_init_find_succeesor(&node1, &node2, &node3).await?;

        // The following are other communications after successful connection

        // 1->3->2 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did3);
        assert_eq!(ev_2.relay.path, vec![did1, did3]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{did,  strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == did1
        ));

        // 1->3 FindSuccessorReport
        // node1 report node3 as node3's successor to node1
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, did1);
        assert_eq!(ev_3.relay.path, vec![did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did3
        ));
        // dht3 won't set did3 as successor
        assert!(!node3.dht.successors().list()?.contains(&did3));

        // 2->3 FindSuccessorReport
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, did2);
        assert_eq!(ev_3.relay.path, vec![did2]);

        // 2->3->1 FindSuccessorReport
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did3);
        assert_eq!(ev_1.relay.path, vec![did2, did3]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did1
        ));
        // dht1 won't set did1 as successor
        assert!(!node1.dht.successors().list()?.contains(&did1));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(swarm1, vec![did2, did3]);
        assert_transports(swarm2, vec![did1, did3]);
        assert_transports(swarm3, vec![did1, did2]);
        assert_eq!(dht1.successors().list()?, vec![did3, did2]);
        assert_eq!(dht2.successors().list()?, vec![did1, did3]);
        assert_eq!(dht3.successors().list()?, vec![did2, did1]);

        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok((node1, node2, node3))
    }

    pub async fn test_listen_join_and_init_find_succeesor(
        node1: &MessageHandler,
        node2: &MessageHandler,
    ) -> Result<()> {
        println!("========================================");
        println!("FN test_listen_join_and_init_find_successor BEGIN");
        println!("========================================");

        let did1 = node1.swarm.did();
        let did2 = node2.swarm.did();

        // check status of successors before join
        let should_update_succ2 = node1.dht.successors().should_insert(did2)?;
        let should_update_succ1 = node2.dht.successors().should_insert(did1)?;

        // 1 JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did1);
        assert_eq!(ev_1.relay.path, vec![did1]);
        assert!(matches!(ev_1.data, Message::JoinDHT(JoinDHT{did, ..}) if did == did2));

        // 2 JoinDHT
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did2);
        assert_eq!(ev_2.relay.path, vec![did2]);
        assert!(matches!(ev_2.data, Message::JoinDHT(JoinDHT{did, ..}) if did == did1));

        #[cfg(feature = "experimental")]
        {
            // 1->2 QueryforTopoInfo
            // if node 2 will insert did1 to it's successor list,
            // It will send query message to node1
            if should_update_succ1 {
                let ev_1 = node1.listen_once().await.unwrap();
                assert!(matches!(
                ev_1.data,
                Message::QueryForTopoInfoSend(QueryForTopoInfoSend{did}) if did == did1
                    ));
            }
        }

        // 1->2 FindSuccessorSend
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did2);
        assert_eq!(ev_1.relay.path, vec![did2]);
        assert!(
            matches!(
                ev_1.data,
                Message::FindSuccessorSend(FindSuccessorSend{did, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect), strict: false}) if did == did2
            ),
            "{:?}",
            ev_1.data
        );

        #[cfg(feature = "experimental")]
        {
            // 2->1 QueryforTopoInfo
            if should_update_succ2 {
                let ev_2 = node2.listen_once().await.unwrap();
                assert!(matches!(
                ev_2.data,
                Message::QueryForTopoInfoSend(QueryForTopoInfoSend{did}) if did == did2
                    ));
            }
        }

        // 2->1 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did1);
        assert_eq!(ev_2.relay.path, vec![did1]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect), strict: false}) if did == did1
        ));
        println!("========================================");
        println!("FN test_listen_join_and_init_find_successor END");
        println!("========================================");

        Ok(())
    }

    pub async fn test_only_two_nodes_establish_connection(
        node1: &MessageHandler,
        node2: &MessageHandler,
	ctx: Option<&TestContext<'_>>
    ) -> Result<()> {
        let did1 = node1.swarm.did();
        let did2 = node2.swarm.did();
        let dht1 = node1.swarm.dht();
        let dht2 = node2.swarm.dht();

        println!("========================================");
        println!("FN test_only_two_nodes_establish_connection BEGIN");
        println!("========================================");

        let should_update_succ2 = node1.dht.successors().should_insert(did2)?;
        let should_update_succ1 = node2.dht.successors().should_insert(did1)?;

        manually_establish_connection(&node1.swarm, &node2.swarm).await?;
        test_listen_join_and_init_find_succeesor(node1, node2).await?;

        #[cfg(feature = "experimental")]
        {
            // 2->1 QueryForTopoInfoReport
            if should_update_succ2 {
                check_update_successor(&node1, Some(&dht2.successors().list()?), ctx).await;
            }
        }

        // 2->1 FindSuccessorReport
        // node2 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did2);
        assert_eq!(ev_1.relay.path, vec![did2]);
        // node2 is only aware of node1, so it respond node1
        assert!(
            matches!(
                ev_1.data,
                Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did1
            ),
            "{:?}",
            ev_1.data
        );
        // dht1 won't set did1 as successor
        assert!(!dht1.successors().list()?.contains(&did1));

        #[cfg(feature = "experimental")]
        {
            if should_update_succ1 {
                // 1->2 QueryForTopoInfoReport
                check_update_successor(&node2, Some(&dht1.successors().list()?), ctx).await;
            }
        }

        // 1->2 FindSuccessorReport
        // node1 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(ev_2.addr, did1);
        assert_eq!(ev_2.relay.path, vec![did1]);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == did2
        ));
        // dht2 won't set did2 as successor
        assert!(!dht2.successors().list()?.contains(&did2));

        println!("========================================");
        println!("FN test_only_two_nodes_establish_connection END");
        println!("========================================");

        Ok(())
    }

    async fn test_connect_via_dht_and_init_find_succeesor(
        node1: &MessageHandler,
        node2: &MessageHandler,
        node3: &MessageHandler,
    ) -> Result<()> {
        let did1 = node1.swarm.did();
        let did2 = node2.swarm.did();
        let did3 = node3.swarm.did();

        // check node1 and node3 is not connected to each other
        assert!(node1.swarm.get_transport(did3).is_none());

        // node1's successor should be node2 now
        assert_eq!(node1.dht.successors().max()?, did2);

        node1.swarm.connect(did3).await.unwrap();

        // node1 send msg to node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did1);
        assert_eq!(ev2.relay.path, vec![did1]);
        assert!(matches!(ev2.data, Message::ConnectNodeSend(_)));

        // node2 relay msg to node3
        let ev3 = node3.listen_once().await.unwrap();
        assert_eq!(ev3.addr, did2);
        assert_eq!(ev3.relay.path, vec![did1, did2]);
        assert!(matches!(ev3.data, Message::ConnectNodeSend(_)));

        // node3 send report to node2
        let ev2 = node2.listen_once().await.unwrap();
        assert_eq!(ev2.addr, did3);
        assert_eq!(ev2.relay.path, vec![did3]);
        assert!(matches!(ev2.data, Message::ConnectNodeReport(_)));

        // node 2 relay report to node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(ev1.addr, did2);
        assert_eq!(ev1.relay.path, vec![did3, did2]);
        assert!(matches!(ev1.data, Message::ConnectNodeReport(_)));

        // assert!(swarm1.get_transport(&did3).is_some());

        // The following are communications after successful connection

        // 1 JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did1);
        assert_eq!(ev_1.relay.path, vec![did1]);
        assert!(matches!(ev_1.data, Message::JoinDHT(JoinDHT{did, ..}) if did == did3));

        // 3 JoinDHT
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, did3);
        assert_eq!(ev_3.relay.path, vec![did3]);
        assert!(matches!(ev_3.data, Message::JoinDHT(JoinDHT{did, ..}) if did == did1));

        // 3->1 FindSuccessorSend
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(ev_1.addr, did3);
        assert_eq!(ev_1.relay.path, vec![did3]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == did3
        ));

        // 1->3 FindSuccessorSend
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(ev_3.addr, did1);
        assert_eq!(ev_3.relay.path, vec![did1]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == did1
        ));

        Ok(())
    }

    fn assert_transports(swarm: Arc<Swarm>, addresses: Vec<Did>) {
        println!(
            "Check transport of {:?}: {:?} for addresses {:?}",
            swarm.did(),
            swarm.get_dids(),
            addresses
        );
        assert_eq!(swarm.get_transports().len(), addresses.len());
        for addr in addresses {
            assert!(swarm.get_transport(addr).is_some());
        }
    }

    #[tokio::test]
    async fn test_quadra_desc_node_connection() -> Result<()> {
	// prepare ctx
	let mut ctx = TestContext::new();

        // 1. node1 to node2
        // 2. node 3 to node 2
        let keys = gen_ordered_keys(4);
        let (key1, key2, key3, key4) = (keys[0], keys[1], keys[2], keys[3]);
        let (node1, node2, node3) = test_triple_ordered_nodes_connection(key1, key2, key3, Some(&ctx)).await?;
        // we now have triple connected node

        let did1 = node1.swarm.did();
        let did2 = node2.swarm.did();
        let did3 = node3.swarm.did();

	ctx.insert(did1, &node1);
	ctx.insert(did2, &node2);
	ctx.insert(did3, &node3);


        let (did4, _, swarm4, node4, _path4) = prepare_node(key4).await;
        // connect node 4 to node2
        manually_establish_connection(&swarm4, &node2.swarm).await?;
        test_listen_join_and_init_find_succeesor(&node4, &node2).await?;
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
            did1, did2, did3, did4,
        );
        println!("==================================================");
        swarm4.connect(did1).await?;
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

        let (did1, dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (did2, dht2, swarm2, node2, _path2) = prepare_node(key2).await;

        // This is only a dummy node for using assert_no_more_msg function
        let (_did3, _dht3, _swarm3, node3, _path3) = prepare_node(key3).await;

        {
            assert!(dht1.lock_finger()?.is_empty());
            assert!(dht1.lock_finger()?.is_empty());
        }

        test_only_two_nodes_establish_connection(&node1, &node2, None).await?;
        assert_no_more_msg(&node1, &node2, &node3).await;

        assert_transports(swarm1.clone(), vec![did2]);
        assert_transports(swarm2.clone(), vec![did1]);
        {
            let finger1 = dht1.lock_finger()?.clone().clone_finger();
            let finger2 = dht2.lock_finger()?.clone().clone_finger();

            assert!(finger1.into_iter().any(|x| x == Some(did2)));
            assert!(finger2.into_iter().any(|x| x == Some(did1)));
        }

        println!("===================================");
        println!("| test disconnect node1 and node2 |");
        println!("===================================");
        swarm1.disconnect(did2).await?;

        // The transport is already dropped by disconnect function.
        // So that we get no msg from this listening.
        let ev1 = node1.listen_once().await;
        assert!(ev1.is_none());

        #[cfg(not(feature = "wasm"))]
        swarm2.get_transport(did1).unwrap().close().await.unwrap();

        for _ in 1..10 {
            println!("wait 3 seconds for node2's transport 2to1 closing");
            sleep(Duration::from_secs(3)).await;
            if let Some(t) = swarm2.get_transport(did1) {
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
        assert_eq!(ev2.addr, did2);
        assert!(matches!(ev2.data, Message::LeaveDHT(LeaveDHT{did}) if did == did1));

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
        let (did1, _dht1, swarm1, node1, _path1) = prepare_node(key1).await;
        let (_did2, _dht2, _swarm2, node2, _path2) = prepare_node(key2).await;
        let (did3, _dht3, swarm3, node3, _path3) = prepare_node(key3).await;
        test_only_two_nodes_establish_connection(&node1, &node2, None).await?;
        assert_no_more_msg(&node1, &node2, &node3).await;

        test_only_two_nodes_establish_connection(&node3, &node2, None).await?;
        assert_no_more_msg(&node1, &node2, &node3).await;
        // Node 1 -- Node 2 -- Node 3
        println!("node1 connect node2 twice here");
        let _ = swarm1.connect(did3).await.unwrap();
        let t_1_3_b = swarm1.connect(did3).await.unwrap();
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

        let t1_3 = swarm1.get_transport(did3).unwrap();
        println!("transport is replace by second");
        assert_eq!(t1_3.id, t_1_3_b.id);
        let t3_1 = swarm3.get_transport(did1).unwrap();

        assert!(t1_3.is_connected().await);
        assert!(t3_1.is_connected().await);
        Ok(())
    }
}
