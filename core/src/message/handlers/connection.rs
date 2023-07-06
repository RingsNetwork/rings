use std::ops::Deref;

use async_recursion::async_recursion;
use async_trait::async_trait;
use futures::future::join_all;

use crate::dht::successor::SuccessorWriter;
use crate::dht::Chord;
use crate::dht::ChordStorageSync;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
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
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;

/// Handler of join dht event from DHT.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub async fn handle_join_dht(act: PeerRingAction) -> Result<Vec<MessageHandlerEvent>> {
    match act {
        PeerRingAction::None => Ok(vec![]),
        // Ask next hop to find successor for did,
        // if there is only two nodes A, B, it may cause loop, for example
        // A's successor is B, B ask A to find successor for B
        // A may send message to it's successor, which is B
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did)) => {
            if next != did {
                Ok(vec![MessageHandlerEvent::SendDirectMessage(
                    Message::FindSuccessorSend(FindSuccessorSend {
                        did,
                        strict: false,
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                    }),
                    next,
                )])
            } else {
                Ok(vec![])
            }
        }
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
            Ok(vec![MessageHandlerEvent::SendDirectMessage(
                Message::QueryForTopoInfoSend(QueryForTopoInfoSend { did: next }),
                next,
            )])
        }
        PeerRingAction::MultiActions(acts) => {
            let ret: Vec<MessageHandlerEvent> = join_all(
                acts.iter()
                    .map(|act| async { handle_join_dht(act.clone()).await }),
            )
            .await
            .iter()
            .filter_map(|x| x.as_ref().ok())
            .flat_map(|xs| xs.iter())
            .cloned()
            .collect();
            Ok(ret)
        }
        _ => unreachable!(),
    }
}

/// When handling update successor, it may cause two situtation, and it may cause multiple situation.
/// 1. DHT put a connected successor into successor list, and ask successor_list of it.
/// 2. DHT wana set a new successor into successor list, but it's not connected, thus it request to connect first.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub async fn handle_update_successor(
    handler: &MessageHandler,
    act: &PeerRingAction,
    ctx: &MessagePayload<Message>,
) -> Result<Vec<MessageHandlerEvent>> {
    match act {
        PeerRingAction::None => Ok(vec![]),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
            Ok(vec![MessageHandlerEvent::SendDirectMessage(
                Message::QueryForTopoInfoSend(QueryForTopoInfoSend { did: *next }),
                *next,
            )])
        }
        PeerRingAction::MultiActions(acts) => {
            let ret: Vec<MessageHandlerEvent> = join_all(
                acts.iter()
                    .map(|act| async { handle_update_successor(handler, act, ctx).await }),
            )
            .await
            .iter()
            .filter_map(|x| x.as_ref().ok())
            .flat_map(|xs| xs.iter())
            .cloned()
            .collect();
            Ok(ret)
        }
        PeerRingAction::RemoteAction(did, PeerRingRemoteAction::TryConnect) => {
            Ok(vec![MessageHandlerEvent::ConnectVia(
                *did,
                ctx.relay.origin_sender(),
            )])
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
    ) -> Result<Vec<MessageHandlerEvent>> {
        let info: TopoInfo = TopoInfo::try_from(self.dht.deref())?;
        if msg.did == self.dht.did {
            Ok(vec![MessageHandlerEvent::SendReportMessage(
                ctx.clone(),
                Message::QueryForTopoInfoReport(QueryForTopoInfoReport { info }),
            )])
        } else {
            Ok(vec![])
        }
    }
}

/// Try join received node into DHT after received from TopoInfo.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<QueryForTopoInfoReport> for MessageHandler {
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &QueryForTopoInfoReport,
    ) -> Result<Vec<MessageHandlerEvent>> {
        let evs: Vec<MessageHandlerEvent> = msg
            .info
            .successors
            .iter()
            .map(|did| MessageHandlerEvent::JoinDHT(*did))
            .collect();
        Ok(evs)
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<LeaveDHT> for MessageHandler {
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &LeaveDHT,
    ) -> Result<Vec<MessageHandlerEvent>> {
        Ok(vec![MessageHandlerEvent::Disconnect(msg.did)])
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<JoinDHT> for MessageHandler {
    async fn handle(
        &self,
        _ctx: &MessagePayload<Message>,
        msg: &JoinDHT,
    ) -> Result<Vec<MessageHandlerEvent>> {
        // here is two situation.
        // finger table just have no other node(beside next), it will be a `create` op
        // otherwise, it will be a `send` op
        Ok(vec![MessageHandlerEvent::JoinDHT(msg.did)])
        // let act = self.dht.join(msg.did)?;
        // handle_join_dht(&self, act, ctx).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeSend> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &ConnectNodeSend,
    ) -> Result<Vec<MessageHandlerEvent>> {
        if self.dht.did != ctx.relay.destination {
            Ok(vec![MessageHandlerEvent::ForwardPayload(ctx.clone(), None)])
        } else {
            Ok(vec![MessageHandlerEvent::AnswerOffer(
                ctx.clone(),
                msg.clone(),
            )])
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &ConnectNodeReport,
    ) -> Result<Vec<MessageHandlerEvent>> {
        if self.dht.did != ctx.relay.destination {
            Ok(vec![MessageHandlerEvent::ForwardPayload(ctx.clone(), None)])
        } else {
            Ok(vec![MessageHandlerEvent::AcceptAnswer(
                ctx.relay.origin_sender(),
                msg.clone(),
            )])
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &FindSuccessorSend,
    ) -> Result<Vec<MessageHandlerEvent>> {
        match self.dht.find_successor(msg.did)? {
            PeerRingAction::Some(did) => {
                if !msg.strict || self.dht.did == msg.did {
                    match &msg.then {
                        FindSuccessorThen::Report(handler) => {
                            Ok(vec![MessageHandlerEvent::SendReportMessage(
                                ctx.clone(),
                                Message::FindSuccessorReport(FindSuccessorReport {
                                    did,
                                    handler: handler.clone(),
                                }),
                            )])
                        }
                    }
                } else {
                    Ok(vec![MessageHandlerEvent::ForwardPayload(
                        ctx.clone(),
                        Some(did),
                    )])
                }
            }
            PeerRingAction::RemoteAction(next, _) => {
                Ok(vec![MessageHandlerEvent::ResetDestination(
                    ctx.clone(),
                    next,
                )])
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FindSuccessorReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &FindSuccessorReport,
    ) -> Result<Vec<MessageHandlerEvent>> {
        if self.dht.did != ctx.relay.destination {
            return Ok(vec![MessageHandlerEvent::ForwardPayload(ctx.clone(), None)]);
        }

        match &msg.handler {
            FindSuccessorReportHandler::FixFingerTable => {
                Ok(vec![MessageHandlerEvent::JoinDHT(msg.did)])
            }
            FindSuccessorReportHandler::Connect => Ok(vec![MessageHandlerEvent::Connect(msg.did)]),
            FindSuccessorReportHandler::SyncStorage => {
                self.dht.successors().update(msg.did)?;
                if let Ok(PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
                )) = self.dht.sync_vnode_with_successor(msg.did).await
                {
                    Ok(vec![MessageHandlerEvent::SendMessage(
                        Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                        next,
                    )])
                } else {
                    Ok(vec![])
                }
            }
            _ => Ok(vec![]),
        }
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
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node;
    use crate::tests::manually_establish_connection;
    use crate::transports::manager::TransportManager;
    use crate::types::ice_transport::IceTransportInterface;

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
    ) -> Result<(Arc<Swarm>, Arc<Swarm>, Arc<Swarm>)> {
        let (node1, _path1) = prepare_node(key1).await;
        let (node2, _path2) = prepare_node(key2).await;
        let (node3, _path3) = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![node1.did()]);
        assert_eq!(node3.dht().successors().list()?, vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3, &node2).await?;
        test_listen_join_and_init_find_succeesor(&node3, &node2).await?;

        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node1.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);

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
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node2.did());
        assert_eq!(ev_3.relay.path, vec![node2.did()]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node3.did()
        ));
        // dht3 won't set did3 as successor
        assert!(!node3.dht().successors().list()?.contains(&node3.did()));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node3.did());
        assert_eq!(ev_2.relay.path, vec![node3.did()]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node2.did()
        ));
        // dht2 won't set did2 as successor
        assert!(!node2.dht().successors().list()?.contains(&node2.did()));

        println!("=== Check state before connect via DHT ===");
        assert_transports(node1.clone(), vec![node2.did()]);
        assert_transports(node2.clone(), vec![node1.did(), node3.did()]);
        assert_transports(node3.clone(), vec![node2.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node1.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        test_connect_via_dht_and_init_find_succeesor(&node1, &node2, &node3).await?;

        // The following are other communications after successful connection

        // 3->1->2 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node1.did());
        assert_eq!(ev_2.relay.path, vec![node3.did(), node1.did()]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == node3.did()
        ));

        // 3->1 FindSuccessorReport
        // node3 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node3.did());
        assert_eq!(ev_1.relay.path, vec![node3.did()]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node1.did()
        ));
        // dht1 won't set did1 as successor
        assert!(!node1.dht().successors().list()?.contains(&node1.did()));

        // 2->1 FindSuccessorReport
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node2.did());
        assert_eq!(ev_1.relay.path, vec![node2.did()]);

        // 2->1->3 FindSuccessorReport
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node1.did());
        assert_eq!(ev_3.relay.path, vec![node2.did(), node1.did()]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node3.did()
        ));
        // dht3 won't set did3 as successor
        assert!(!node3.dht().successors().list()?.contains(&node3.did()));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(node1.clone(), vec![node2.did(), node3.did()]);
        assert_transports(node2.clone(), vec![node1.did(), node3.did()]);
        assert_transports(node3.clone(), vec![node1.did(), node2.did()]);
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
        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok((node1.clone(), node2.clone(), node3.clone()))
    }

    async fn test_triple_desc_ordered_nodes_connection(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<(Arc<Swarm>, Arc<Swarm>, Arc<Swarm>)> {
        let (node1, _path1) = prepare_node(key1).await;
        let (node2, _path2) = prepare_node(key2).await;
        let (node3, _path3) = prepare_node(key3).await;

        println!("========================================");
        println!("||  now we connect node1 and node2    ||");
        println!("========================================");

        test_only_two_nodes_establish_connection(&node1, &node2).await?;

        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![node1.did()]);
        assert_eq!(node3.dht().successors().list()?, vec![]);

        println!("========================================");
        println!("||  now we start join node3 to node2  ||");
        println!("========================================");

        manually_establish_connection(&node3, &node2).await?;
        test_listen_join_and_init_find_succeesor(&node3, &node2).await?;

        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);

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
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node2.did());
        assert_eq!(ev_1.relay.path, vec![node3.did(), node2.did()]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == node3.did()
        ));

        // 3->2 FindSuccessorReport
        // node3 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node3.did());
        assert_eq!(ev_2.relay.path, vec![node3.did()]);
        // node3 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node2.did()
        ));
        // dht2 won't set did2 as successor
        assert!(!node2.dht().successors().list()?.contains(&node2.did()));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node3's successor to node2
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node1.did());
        assert_eq!(ev_2.relay.path, vec![node1.did()]);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node2.did()
        ));

        // 1->2->3 FindSuccessorReport
        // node2 relay report to node3
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node2.did());
        assert_eq!(ev_3.relay.path, vec![node1.did(), node2.did()]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node2.did()
        ));

        println!("=== Check state before connect via DHT ===");
        assert_transports(node1.clone(), vec![node2.did()]);
        assert_transports(node2.clone(), vec![node1.did(), node3.did()]);
        assert_transports(node3.clone(), vec![node2.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node1.did(),
            node3.did()
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        test_connect_via_dht_and_init_find_succeesor(&node1, &node2, &node3).await?;

        // The following are other communications after successful connection

        // 1->3->2 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node3.did());
        assert_eq!(ev_2.relay.path, vec![node1.did(), node3.did()]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{did,  strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == node1.did()
        ));

        // 1->3 FindSuccessorReport
        // node1 report node3 as node3's successor to node1
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node1.did());
        assert_eq!(ev_3.relay.path, vec![node1.did()]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node3.did()
        ));
        // dht3 won't set did3 as successor
        assert!(!node3.dht.successors().list()?.contains(&node3.did()));

        // 2->3 FindSuccessorReport
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node2.did());
        assert_eq!(ev_3.relay.path, vec![node2.did()]);

        // 2->3->1 FindSuccessorReport
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node3.did());
        assert_eq!(ev_1.relay.path, vec![node2.did(), node3.did()]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node1.did()
        ));
        // dht1 won't set did1 as successor
        assert!(!node1.dht.successors().list()?.contains(&node1.did()));

        assert_no_more_msg(&node1, &node2, &node3).await;

        println!("=== Check state after connect via DHT ===");
        assert_transports(node1.clone(), vec![node2.did(), node3.did()]);
        assert_transports(node2.clone(), vec![node1.did(), node3.did()]);
        assert_transports(node3.clone(), vec![node1.did(), node2.did()]);
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

        tokio::fs::remove_dir_all("./tmp").await.ok();
        Ok((node1.clone(), node2.clone(), node3.clone()))
    }

    pub async fn test_listen_join_and_init_find_succeesor(
        node1: &Swarm,
        node2: &Swarm,
    ) -> Result<()> {
        // 1 JoinDHT
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node1.did());
        assert_eq!(ev_1.relay.path, vec![node1.did()]);
        assert!(matches!(ev_1.data, Message::JoinDHT(JoinDHT{did, ..}) if did == node2.did()));

        // 2 JoinDHT
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node2.did());
        assert_eq!(ev_2.relay.path, vec![node2.did()]);
        assert!(matches!(ev_2.data, Message::JoinDHT(JoinDHT{did, ..}) if did == node1.did()));

        // 1->2 FindSuccessorSend
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node2.did());
        assert_eq!(ev_1.relay.path, vec![node2.did()]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect), strict: false}) if did == node2.did()
        ));

        // 2->1 FindSuccessorSend
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node1.did());
        assert_eq!(ev_2.relay.path, vec![node1.did()]);
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect), strict: false}) if did == node1.did()
        ));

        Ok(())
    }

    pub async fn test_only_two_nodes_establish_connection(
        node1: &Swarm,
        node2: &Swarm,
    ) -> Result<()> {
        manually_establish_connection(node1, node2).await?;
        test_listen_join_and_init_find_succeesor(node1, node2).await?;

        // 2->1 FindSuccessorReport
        // node2 report node1 as node1's successor to node1
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node2.did());
        assert_eq!(ev_1.relay.path, vec![node2.did()]);
        // node2 is only aware of node1, so it respond node1
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node1.did()
        ));
        // dht1 won't set dhd1 as successor
        assert!(!node1.dht().successors().list()?.contains(&node1.did()));

        // 1->2 FindSuccessorReport
        // node1 report node2 as node2's successor to node2
        let ev_2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev_2.addr, node1.did());
        assert_eq!(ev_2.relay.path, vec![node1.did()]);
        // node1 is only aware of node2, so it respond node2
        assert!(matches!(
            ev_2.data,
            Message::FindSuccessorReport(FindSuccessorReport{did, handler: FindSuccessorReportHandler::Connect}) if did == node2.did()
        ));
        // dht2 won't set did2 as successor
        assert!(!node2.dht().successors().list()?.contains(&node2.did()));

        Ok(())
    }

    async fn test_connect_via_dht_and_init_find_succeesor(
        node1: &Swarm,
        node2: &Swarm,
        node3: &Swarm,
    ) -> Result<()> {
        // check node1 and node3 is not connected to each other
        assert!(node1.get_transport(node3.did()).is_none());

        // node1's successor should be node2 now
        assert_eq!(node1.dht.successors().max()?, node2.did());

        node1.connect(node3.did()).await.unwrap();

        // node1 send msg to node2
        let ev2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev2.addr, node1.did());
        assert_eq!(ev2.relay.path, vec![node1.did()]);
        assert!(matches!(ev2.data, Message::ConnectNodeSend(_)));

        // node2 relay msg to node3
        let ev3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev3.addr, node2.did());
        assert_eq!(ev3.relay.path, vec![node1.did(), node2.did()]);
        assert!(matches!(ev3.data, Message::ConnectNodeSend(_)));

        // node3 send report to node2
        let ev2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev2.addr, node3.did());
        assert_eq!(ev2.relay.path, vec![node3.did()]);
        assert!(matches!(ev2.data, Message::ConnectNodeReport(_)));

        // node 2 relay report to node1
        let ev1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev1.addr, node2.did());
        assert_eq!(ev1.relay.path, vec![node3.did(), node2.did()]);
        assert!(matches!(ev1.data, Message::ConnectNodeReport(_)));

        // The following are communications after successful connection

        // 1 JoinDHT
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node1.did());
        assert_eq!(ev_1.relay.path, vec![node1.did()]);
        assert!(matches!(ev_1.data, Message::JoinDHT(JoinDHT{did, ..}) if did == node3.did()));

        // 3 JoinDHT
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node3.did());
        assert_eq!(ev_3.relay.path, vec![node3.did()]);
        assert!(matches!(ev_3.data, Message::JoinDHT(JoinDHT{did, ..}) if did == node1.did()));

        // 3->1 FindSuccessorSend
        let ev_1 = node1.listen_once().await.unwrap().0;
        assert_eq!(ev_1.addr, node3.did());
        assert_eq!(ev_1.relay.path, vec![node3.did()]);
        assert!(matches!(
            ev_1.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == node3.did()
        ));

        // 1->3 FindSuccessorSend
        let ev_3 = node3.listen_once().await.unwrap().0;
        assert_eq!(ev_3.addr, node1.did());
        assert_eq!(ev_3.relay.path, vec![node1.did()]);
        assert!(matches!(
            ev_3.data,
            Message::FindSuccessorSend(FindSuccessorSend{did, strict: false, then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect)}) if did == node1.did()
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
        // 1. node1 to node2
        // 2. node 3 to node 2
        let keys = gen_ordered_keys(4);
        let (key1, key2, key3, key4) = (keys[0], keys[1], keys[2], keys[3]);
        let (node1, node2, node3) = test_triple_ordered_nodes_connection(key1, key2, key3).await?;
        // we now have triple connected node

        let (node4, _path4) = prepare_node(key4).await;
        // connect node 4 to node2
        manually_establish_connection(&node4, &node2).await?;
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
            "node1.did(): {:?}, node2.did(): {:?}, node3.did(): {:?}, node4.did(): {:?}",
            node1.did(),
            node2.did(),
            node3.did(),
            node4.did(),
        );
        println!("==================================================");
        node4.connect(node1.did()).await?;
        // node 4 send msg to node2
        assert!(matches!(
            node2.listen_once().await.unwrap().0.data,
            Message::ConnectNodeSend(_)
        ));
        // node 2 relay to node 2
        assert!(matches!(
            node1.listen_once().await.unwrap().0.data,
            Message::ConnectNodeSend(_)
        ));
        // report to node 2
        assert!(matches!(
            node2.listen_once().await.unwrap().0.data,
            Message::ConnectNodeReport(_)
        ));
        // report to node 4
        assert!(matches!(
            node4.listen_once().await.unwrap().0.data,
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

        let (node1, _path1) = prepare_node(key1).await;
        let (node2, _path2) = prepare_node(key2).await;

        // This is only a dummy node for using assert_no_more_msg function
        let (node3, _path3) = prepare_node(key3).await;

        {
            assert!(node1.dht().lock_finger()?.is_empty());
            assert!(node1.dht().lock_finger()?.is_empty());
        }

        test_only_two_nodes_establish_connection(&node1, &node2).await?;
        assert_no_more_msg(&node1, &node2, &node3).await;

        assert_transports(node1.clone(), vec![node2.did()]);
        assert_transports(node2.clone(), vec![node1.did()]);
        {
            let finger1 = node1.dht().lock_finger()?.clone().clone_finger();
            let finger2 = node2.dht().lock_finger()?.clone().clone_finger();

            assert!(finger1.into_iter().any(|x| x == Some(node2.did())));
            assert!(finger2.into_iter().any(|x| x == Some(node1.did())));
        }

        println!("===================================");
        println!("| test disconnect node1 and node2 |");
        println!("===================================");
        node1.disconnect(node2.did()).await?;

        // The transport is already dropped by disconnect function.
        // So that we get no msg from this listening.
        let ev1 = node1.listen_once().await;
        assert!(ev1.is_none());

        #[cfg(not(feature = "wasm"))]
        node2
            .get_transport(node1.did())
            .unwrap()
            .close()
            .await
            .unwrap();

        for _ in 1..10 {
            println!("wait 3 seconds for node2's transport 2to1 closing");
            sleep(Duration::from_secs(3)).await;
            if let Some(t) = node2.get_transport(node1.did()) {
                if t.is_disconnected().await {
                    println!("transport 2to1 is disconnected!!!!");
                    break;
                }
            } else {
                println!("transport 2to1 is disappeared!!!!");
                break;
            }
        }
        let ev2 = node2.listen_once().await.unwrap().0;
        assert_eq!(ev2.addr, node2.did());
        assert!(matches!(ev2.data, Message::LeaveDHT(LeaveDHT{did}) if did == node1.did()));

        assert_no_more_msg(&node1, &node2, &node3).await;

        assert_transports(node1.clone(), vec![]);
        assert_transports(node2.clone(), vec![]);
        {
            let finger1 = node1.dht().lock_finger()?.clone().clone_finger();
            let finger2 = node2.dht().lock_finger()?.clone().clone_finger();
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
        let (node1, _path1) = prepare_node(key1).await;
        let (node2, _path2) = prepare_node(key2).await;
        let (node3, _path3) = prepare_node(key3).await;
        test_only_two_nodes_establish_connection(&node1, &node2).await?;
        assert_no_more_msg(&node1, &node2, &node3).await;

        test_only_two_nodes_establish_connection(&node3, &node2).await?;
        assert_no_more_msg(&node1, &node2, &node3).await;
        // Node 1 -- Node 2 -- Node 3
        println!("node1 connect node2 twice here");
        let _ = node1.connect(node3.did()).await.unwrap();
        let t_1_3_b = node1.connect(node3.did()).await.unwrap();
        // ConnectNodeSend
        let _ = node2.listen_once().await.unwrap();
        let _ = node2.listen_once().await.unwrap();
        // ConnectNodeSend
        let _ = node3.listen_once().await.unwrap();
        let _ = node3.listen_once().await.unwrap();
        // ConnectNodeReport
        // `self.push_pending_transport(&trans)?;`
        let _ = node2.listen_once().await.unwrap();
        let _ = node2.listen_once().await.unwrap();
        // ConnectNodeReport
        // self.register(&relay.sender(), transport).await
        let _ = node1.listen_once().await.unwrap();
        let _ = node1.listen_once().await.unwrap();
        println!("wait for handshake here");
        sleep(Duration::from_secs(3)).await;
        // transport got from node1 for node3
        // transport got from node3 for node
        // JoinDHT twice here
        let ev3 = node3.listen_once().await.unwrap().0;
        assert!(matches!(ev3.data, Message::JoinDHT(_)));
        let _ = node3.listen_once().await.is_none();

        // JoinDHT twice here
        let ev1 = node1.listen_once().await.unwrap().0;
        assert!(matches!(ev1.data, Message::JoinDHT(_)));
        let _ = node1.listen_once().await.is_none();

        let t1_3 = node1.get_transport(node3.did()).unwrap();
        println!("transport is replace by second");
        assert_eq!(t1_3.id, t_1_3_b.id);
        let t3_1 = node3.get_transport(node1.did()).unwrap();

        assert!(t1_3.is_connected().await);
        assert!(t3_1.is_connected().await);
        Ok(())
    }
}
