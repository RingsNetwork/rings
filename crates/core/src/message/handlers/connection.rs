use async_trait::async_trait;

use crate::dht::successor::SuccessorReader;
use crate::dht::topology;
use crate::dht::types::Chord;
use crate::dht::types::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::PayloadRelayFunctor;
use crate::message::types::ConnectNodeReport;
use crate::message::types::ConnectNodeSend;
use crate::message::types::FindSuccessorReport;
use crate::message::types::FindSuccessorSend;
use crate::message::types::Message;
use crate::message::types::PeerLivenessProbe;
use crate::message::types::PeerLivenessReport;
use crate::message::types::QueryForTopoInfoReport;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::types::Then;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

fn confirmed_topology(info: &TopoInfo, is_active: impl Fn(Did) -> bool) -> TopoInfo {
    TopoInfo {
        successors: info
            .successors
            .iter()
            .copied()
            .filter(|peer| is_active(*peer))
            .collect(),
        predecessor: info.predecessor.filter(|peer| is_active(*peer)),
    }
}

fn topology_has_confirmed_peer(info: &TopoInfo) -> bool {
    info.predecessor.is_some() || !info.successors.is_empty()
}

fn connect_successor_hint(dht: &PeerRing, requester: Did, reported: Did) -> Result<Did> {
    if reported != requester {
        return Ok(reported);
    }

    let mut candidates = dht.successors().list()?;
    candidates.push(dht.did);
    candidates.retain(|candidate| *candidate != requester);

    Ok(topology::successors(&candidates, requester, 1)
        .into_iter()
        .next()
        .unwrap_or(reported))
}

/// PeerLivenessProbe is a direct overlay liveness probe.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<PeerLivenessProbe> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &PeerLivenessProbe) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
                .await;
        }

        self.run_effects([PayloadRelayFunctor::send_report_message(
            ctx,
            Message::PeerLivenessReport(msg.resp()),
        )
        .into()])
            .await
    }
}

/// PeerLivenessReport is handled by the callback's verified-inbound liveness update.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<PeerLivenessReport> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload, _msg: &PeerLivenessReport) -> Result<()> {
        Ok(())
    }
}

/// QueryForTopoInfoSend is direct message
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &QueryForTopoInfoSend) -> Result<()> {
        let info: TopoInfo = TopoInfo::try_from(self.dht.as_ref())?;
        if msg.targets(self.dht.did) {
            self.run_effects([PayloadRelayFunctor::send_report_message(
                ctx,
                Message::QueryForTopoInfoReport(msg.resp(info)),
            )
            .into()])
                .await?
        }
        Ok(())
    }
}

/// Try join received node into DHT after received from TopoInfo.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoReport> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload, msg: &QueryForTopoInfoReport) -> Result<()> {
        match msg.then {
            <QueryForTopoInfoReport as Then>::Then::SyncSuccessor => {
                let successors = msg.info.successors.clone();
                self.connect_dht_peers(successors.iter().copied()).await?;
                for peer in successors {
                    if self.transport.get_connection(peer).is_some() {
                        self.join_dht(peer).await?;
                    }
                }
            }
            <QueryForTopoInfoReport as Then>::Then::Stabilization => {
                // Candidates begin as non-routable pending handshakes. Only
                // peers whose data channel has opened may enter the DHT view.
                let candidates = msg
                    .info
                    .predecessor
                    .into_iter()
                    .chain(msg.info.successors.iter().copied());
                self.connect_dht_peers(candidates).await?;

                let confirmed = confirmed_topology(&msg.info, |peer| {
                    self.transport.get_connection(peer).is_some()
                });
                if topology_has_confirmed_peer(&confirmed) {
                    let ev = self.dht.stabilize(confirmed)?;
                    self.handle_dht_events(&ev).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ConnectNodeSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ConnectNodeSend) -> Result<()> {
        if !self.transport.accepts_connection_offer(msg) {
            tracing::warn!(
                local = %self.dht.did,
                tx_id = %ctx.transaction.tx_id,
                origin = ?ctx.relay.try_origin_sender().ok(),
                relay_destination = %ctx.relay.destination,
                transaction_destination = %ctx.transaction.destination,
                mode = ?msg.dht_protocol_mode(),
                "CONNECT_NODE offer rejected by DHT protocol mismatch"
            );
            return Ok(());
        }

        if ctx.should_forward_from(self.dht.did) {
            tracing::info!(
                local = %self.dht.did,
                tx_id = %ctx.transaction.tx_id,
                origin = ?ctx.relay.try_origin_sender().ok(),
                next_hop = %ctx.relay.next_hop,
                relay_destination = %ctx.relay.destination,
                transaction_destination = %ctx.transaction.destination,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE offer forward"
            );
            self.run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
                .await
        } else {
            let peer = ctx.relay.try_origin_sender()?;
            tracing::info!(
                local = %self.dht.did,
                peer = %peer,
                tx_id = %ctx.transaction.tx_id,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE offer answer start"
            );
            let answer = match self
                .transport
                .answer_remote_connection(peer, self.inner_callback(), msg)
                .await
            {
                Ok(answer) => {
                    tracing::info!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        sdp_bytes = answer.sdp.len(),
                        "CONNECT_NODE offer answer complete"
                    );
                    answer
                }
                Err(error) => {
                    tracing::warn!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        error = ?error,
                        "CONNECT_NODE offer answer failed"
                    );
                    return Err(error);
                }
            };
            self.run_effects([PayloadRelayFunctor::send_report_message(
                ctx,
                Message::ConnectNodeReport(answer),
            )
            .into()])
                .await
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ConnectNodeReport) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            tracing::info!(
                local = %self.dht.did,
                tx_id = %ctx.transaction.tx_id,
                origin = ?ctx.relay.try_origin_sender().ok(),
                next_hop = %ctx.relay.next_hop,
                relay_destination = %ctx.relay.destination,
                transaction_destination = %ctx.transaction.destination,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE answer forward"
            );
            self.run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
                .await
        } else {
            let peer = ctx.relay.try_origin_sender()?;
            tracing::info!(
                local = %self.dht.did,
                peer = %peer,
                tx_id = %ctx.transaction.tx_id,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE answer accept start"
            );
            match self.transport.accept_remote_connection(peer, msg).await {
                Ok(()) => {
                    tracing::info!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        "CONNECT_NODE answer accept complete"
                    );
                    Ok(())
                }
                Err(error) => {
                    tracing::warn!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        error = ?error,
                        "CONNECT_NODE answer accept failed"
                    );
                    Err(error)
                }
            }
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FindSuccessorSend) -> Result<()> {
        match self.dht.find_successor(msg.did)? {
            PeerRingAction::Some(did) => {
                if msg.accepts_local_successor(self.dht.did) {
                    match &msg.then {
                        FindSuccessorThen::Report(handler) => {
                            let did = match handler {
                                FindSuccessorReportHandler::Connect => connect_successor_hint(
                                    self.dht.as_ref(),
                                    ctx.relay.try_origin_sender()?,
                                    did,
                                )?,
                                _ => did,
                            };
                            self.run_effects([PayloadRelayFunctor::send_report_message(
                                ctx,
                                Message::FindSuccessorReport(FindSuccessorReport {
                                    did,
                                    handler: handler.clone(),
                                }),
                            )
                            .into()])
                                .await
                        }
                    }
                } else {
                    self.run_effects([PayloadRelayFunctor::forward_payload(ctx, Some(did)).into()])
                        .await
                }
            }
            PeerRingAction::RemoteAction(next, _) => {
                self.run_effects([PayloadRelayFunctor::reset_destination(ctx, next).into()])
                    .await
            }
            act => Err(Error::unexpected_peer_ring_action(act)),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindSuccessorReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FindSuccessorReport) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
                .await;
        }

        match &msg.handler {
            FindSuccessorReportHandler::FixFingerTable { index } => {
                if self.transport.get_connection(msg.did).is_some() {
                    self.dht.apply_fixed_finger(*index, msg.did)?;
                } else if msg.reports_remote_successor(self.dht.did) {
                    self.connect_dht_peer(msg.did).await?;
                    if self.transport.get_connection(msg.did).is_some() {
                        self.dht.apply_fixed_finger(*index, msg.did)?;
                    }
                }
            }
            FindSuccessorReportHandler::Connect if msg.reports_remote_successor(self.dht.did) => {
                self.connect_dht_peer(msg.did).await?;
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
pub mod tests {
    //! tests
    use tokio::time::sleep;
    use tokio::time::Duration;

    use super::*;
    use crate::dht::successor::SuccessorReader;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::tests::default::assert_no_more_msg;
    use crate::tests::default::gen_pure_dht;
    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::default::Node;
    use crate::tests::manually_establish_connection;

    #[test]
    fn topology_report_keeps_only_confirmed_peers() {
        let active = SecretKey::random().address().into();
        let pending_successor = SecretKey::random().address().into();
        let pending_predecessor = SecretKey::random().address().into();
        let confirmed = confirmed_topology(
            &TopoInfo {
                successors: vec![active, pending_successor],
                predecessor: Some(pending_predecessor),
            },
            |peer| peer == active,
        );

        assert_eq!(confirmed.successors, vec![active]);
        assert_eq!(confirmed.predecessor, None);
        assert!(topology_has_confirmed_peer(&confirmed));
    }

    #[test]
    fn connect_successor_hint_skips_requester_self_report() -> Result<()> {
        let keys = gen_ordered_keys(4);
        let local = keys[0].address().into();
        let requester = keys[1].address().into();
        let next = keys[2].address().into();
        let tail = keys[3].address().into();
        let dht = gen_pure_dht(local);

        dht.join(next)?;
        dht.join(tail)?;
        dht.join(requester)?;

        assert_eq!(dht.successors().list()?, vec![requester, next, tail]);
        assert_eq!(connect_successor_hint(&dht, requester, requester)?, next);
        Ok(())
    }

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
        if node1.swarm.transport.get_connection(node3.did()).is_some() {
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
        } else {
            node1.assert_transports(vec![node2.did()]);
            node2.assert_transports(vec![node1.did(), node3.did()]);
            node3.assert_transports(vec![node2.did()]);
            assert_eq!(node1.dht().successors().list()?, vec![node2.did(),]);
            assert_eq!(node2.dht().successors().list()?, vec![
                node3.did(),
                node1.did()
            ]);
            assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);
        }

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        if node1.swarm.transport.get_connection(node3.did()).is_none() {
            node1.swarm.connect(node3.did()).await?;
        }
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
        if node1.swarm.transport.get_connection(node3.did()).is_some() {
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
        } else {
            node1.assert_transports(vec![node2.did()]);
            node2.assert_transports(vec![node1.did(), node3.did()]);
            node3.assert_transports(vec![node2.did()]);
            assert_eq!(node1.dht().successors().list()?, vec![node2.did()]);
            assert_eq!(node2.dht().successors().list()?, vec![
                node1.did(),
                node3.did()
            ]);
            assert_eq!(node3.dht().successors().list()?, vec![node2.did()]);
        }

        println!("=============================================");
        println!("||  now we connect node1 to node3 via DHT  ||");
        println!("=============================================");

        if node1.swarm.transport.get_connection(node3.did()).is_none() {
            node1.swarm.connect(node3.did()).await?;
        }
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
        // Poll for convergence rather than sleeping a fixed amount: under the
        // release-LTO CI run with native WebRTC, 6s is not always enough and the
        // assertions below would flake. The expected final state is unchanged.
        wait_until_with_state(
            "node4 joined: DHT successors converged",
            || {
                Ok(
                    node1.dht().successors().list()? == vec![node2.did(), node3.did(), node4.did()]
                        && node2.dht().successors().list()?
                            == vec![node3.did(), node4.did(), node1.did()]
                        && node3.dht().successors().list()?
                            == vec![node4.did(), node1.did(), node2.did()]
                        && node4.dht().successors().list()?
                            == vec![node1.did(), node2.did(), node3.did()],
                )
            },
            || describe_nodes([&node1, &node2, &node3, &node4]),
        )
        .await?;

        println!("=== Check state before connect via DHT ===");
        node1.assert_transports(vec![node2.did(), node3.did(), node4.did()]);
        node2.assert_transports(vec![node3.did(), node4.did(), node1.did()]);
        node3.assert_transports(vec![node4.did(), node1.did(), node2.did()]);
        node4.assert_transports(vec![node1.did(), node2.did(), node3.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![
            node2.did(),
            node3.did(),
            node4.did(),
        ]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node4.did(),
            node1.did(),
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![
            node4.did(),
            node1.did(),
            node2.did(),
        ]);
        assert_eq!(node4.dht().successors().list()?, vec![
            node1.did(),
            node2.did(),
            node3.did(),
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

        if node4.swarm.transport.get_connection(node3.did()).is_none() {
            node4.swarm.connect(node3.did()).await?;
        }
        // Same as above: poll for the post-connect converged state instead of a
        // fixed 6s sleep so the test is robust under CI contention.
        wait_until_with_state(
            "node4 connected node3: DHT successors converged",
            || {
                Ok(
                    node1.dht().successors().list()? == vec![node2.did(), node3.did(), node4.did()]
                        && node2.dht().successors().list()?
                            == vec![node3.did(), node4.did(), node1.did()]
                        && node3.dht().successors().list()?
                            == vec![node4.did(), node1.did(), node2.did()]
                        && node4.dht().successors().list()?
                            == vec![node1.did(), node2.did(), node3.did()],
                )
            },
            || describe_nodes([&node1, &node2, &node3, &node4]),
        )
        .await?;

        println!("=== Check state after connect via DHT ===");
        node1.assert_transports(vec![node2.did(), node3.did(), node4.did()]);
        node2.assert_transports(vec![node3.did(), node4.did(), node1.did()]);
        node3.assert_transports(vec![node4.did(), node1.did(), node2.did()]);
        node4.assert_transports(vec![node1.did(), node2.did(), node3.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![
            node2.did(),
            node3.did(),
            node4.did()
        ]);
        assert_eq!(node2.dht().successors().list()?, vec![
            node3.did(),
            node4.did(),
            node1.did(),
        ]);
        assert_eq!(node3.dht().successors().list()?, vec![
            node4.did(),
            node1.did(),
            node2.did(),
        ]);
        assert_eq!(node4.dht().successors().list()?, vec![
            node1.did(),
            node2.did(),
            node3.did(),
        ]);

        Ok(())
    }

    #[cfg(feature = "dummy")]
    #[tokio::test]
    async fn joining_between_bootstrap_and_successor_connects_successor_hint() -> Result<()> {
        let keys = gen_ordered_keys(4);
        let (node1, node2, node3) =
            test_triple_ordered_nodes_connection(keys[0], keys[2], keys[3]).await?;
        let joining = prepare_node(keys[1]).await;

        manually_establish_connection(&joining.swarm, &node1.swarm).await;
        wait_until(
            "joining peer connects past bootstrap successor self-report",
            || {
                Ok(joining
                    .swarm
                    .transport
                    .get_connection(node2.did())
                    .is_some())
            },
        )
        .await?;

        wait_for_msgs([&node1, &node2, &node3, &joining]).await;
        assert_no_more_msg([&node1, &node2, &node3, &joining]).await;

        joining.assert_transports(vec![node1.did(), node2.did(), node3.did()]);
        assert_eq!(node1.dht().successors().list()?, vec![
            joining.did(),
            node2.did(),
            node3.did(),
        ]);
        assert_eq!(joining.dht().successors().list()?, vec![
            node2.did(),
            node3.did(),
            node1.did(),
        ]);

        Ok(())
    }

    /// Poll `cond` every 200ms until it returns true, failing after ~60s.
    /// Used instead of fixed sleeps so the test is deterministic regardless of
    /// how long the WebRTC handshake/teardown takes on a given machine.
    ///
    /// The window is generous on purpose: ICE paces connectivity checks at
    /// ~200ms each, so on a host with many network interfaces (lots of
    /// candidate pairs) establishing the connection can legitimately take ~20s.
    async fn wait_until(msg: &str, mut cond: impl FnMut() -> Result<bool>) -> Result<()> {
        wait_until_with_state(msg, &mut cond, String::new).await
    }

    async fn wait_until_with_state(
        msg: &str,
        mut cond: impl FnMut() -> Result<bool>,
        state: impl Fn() -> String,
    ) -> Result<()> {
        for _ in 0..300 {
            if cond()? {
                return Ok(());
            }
            sleep(Duration::from_millis(200)).await;
        }
        let state = state();
        if state.is_empty() {
            Err(Error::InvalidMessage(format!("timeout waiting for: {msg}")))
        } else {
            Err(Error::InvalidMessage(format!(
                "timeout waiting for: {msg}\n{state}"
            )))
        }
    }

    fn describe_nodes<'a>(nodes: impl IntoIterator<Item = &'a Node>) -> String {
        nodes
            .into_iter()
            .map(|node| {
                format!(
                    "{:?}: successors={:?}, transports={:?}",
                    node.did(),
                    node.dht().successors().list().unwrap_or_default(),
                    node.swarm.transport.get_connection_ids(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
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

        // The data channels open and `on_data_channel_open -> join_dht` runs
        // asynchronously, so poll until both sides have joined each other rather
        // than asserting after a fixed wait.
        wait_until("node1 and node2 to join each other's DHT", || {
            let finger1 = node1.dht().lock_finger()?.clone().clone_finger();
            let finger2 = node2.dht().lock_finger()?.clone().clone_finger();
            Ok(finger1.into_iter().any(|x| x == Some(node2.did()))
                && finger2.into_iter().any(|x| x == Some(node1.did())))
        })
        .await?;

        node1.assert_transports(vec![node2.did()]);
        node2.assert_transports(vec![node1.did()]);

        println!("===================================");
        println!("| test disconnect node1 and node2 |");
        println!("===================================");
        node1.swarm.disconnect(node2.did()).await?;

        // node1 closes locally; node2 learns via the data channel closing and
        // tears its side down promptly (without waiting for the ICE `Failed`
        // timeout). Poll until both sides have removed the connection.
        wait_until("both sides to drop the connection", || {
            Ok(node1.swarm.transport.get_connection(node2.did()).is_none()
                && node2.swarm.transport.get_connection(node1.did()).is_none())
        })
        .await?;

        node1.assert_transports(vec![]);
        node2.assert_transports(vec![]);

        wait_until("both sides to remove each other from DHT fingers", || {
            let finger1 = node1.dht().lock_finger()?.clone().clone_finger();
            let finger2 = node2.dht().lock_finger()?.clone().clone_finger();
            Ok(
                finger1.into_iter().all(|x| x.is_none())
                    && finger2.into_iter().all(|x| x.is_none()),
            )
        })
        .await?;

        Ok(())
    }
}
