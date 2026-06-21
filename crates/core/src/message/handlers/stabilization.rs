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

// Driven over the dummy transport's explicit delivery queue (`dummy::controlled`), not real webrtc
// + a wall-clock quiescence guess. The old real-webrtc version was flaky: stabilization makes the
// outer nodes connect to each other, and the WebRTC *answer* SDP they exchange is produced by ICE
// gathering against a real STUN server, whose latency is unbounded — so a late `ConnectNodeReport`
// could land after `wait_for_msgs`'s 3s "quiet" gap and trip `assert_no_more_msg`. The controlled
// queue removes ICE/STUN and the wall clock entirely: delivery is explicit and drained to
// quiescence, so the converged state is reached deterministically (cf. `dht_schedule`).
#[cfg(feature = "dummy")]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use rings_transport::connections::dummy_controlled;

    use super::*;
    use crate::dht::successor::SuccessorReader;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node;
    use crate::tests::manually_establish_connection;

    /// Deliver every queued message (oldest first) until the controlled queue is empty — the
    /// deterministic replacement for `wait_for_msgs` + `assert_no_more_msg`. Quiescence is exact
    /// (queue empty), not a timed guess.
    async fn drain() {
        let mut delivered = 0usize;
        while dummy_controlled::pending() > 0 {
            dummy_controlled::deliver(0).await;
            delivered += 1;
            assert!(
                delivered < 1_000_000,
                "runaway delivery — routing self-route loop"
            );
        }
    }

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

    /// Bring up the triangle (node1↔node2, node3↔node2) and stabilize to quiescence over the
    /// controlled queue, then assert the converged ring state. `before_*` is the deterministic
    /// bootstrap successor state (depends only on the ring positions of the three DIDs); `succ_*` /
    /// `pred_*` are the unique converged fixpoint. Asserting bootstrap + fixpoint (rather than each
    /// brittle per-round intermediate, which is delivery-order dependent) is what makes this sound.
    #[allow(clippy::too_many_arguments)]
    async fn run_triple_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
        before2: Vec<usize>,
        succ: [Vec<usize>; 3],
        pred: [usize; 3],
    ) -> Result<()> {
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;
        let node3 = prepare_node(key3).await;
        let dids = [node1.did(), node2.did(), node3.did()];

        dummy_controlled::enable(true);

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        manually_establish_connection(&node3.swarm, &node2.swarm).await;
        drain().await;

        // Bootstrap state: node1/node3 each know only node2; node2 knows both, ordered by ring
        // position; no predecessors yet.
        assert_eq!(node1.dht().successors().list()?, vec![dids[1]]);
        assert_eq!(
            node2.dht().successors().list()?,
            before2.iter().map(|&i| dids[i]).collect::<Vec<_>>()
        );
        assert_eq!(node3.dht().successors().list()?, vec![dids[1]]);
        assert!(node1.dht().lock_predecessor()?.is_none());
        assert!(node2.dht().lock_predecessor()?.is_none());
        assert!(node3.dht().lock_predecessor()?.is_none());

        // Stabilize all nodes, draining to quiescence each round, until the ring converges.
        for _ in 0..10 {
            run_stabilization_once(node1.swarm.clone()).await?;
            run_stabilization_once(node2.swarm.clone()).await?;
            run_stabilization_once(node3.swarm.clone()).await?;
            drain().await;
        }

        dummy_controlled::enable(false);

        let nodes = [&node1, &node2, &node3];
        for (n, expected) in nodes.iter().zip(succ.iter()) {
            assert_eq!(
                n.dht().successors().list()?,
                expected.iter().map(|&i| dids[i]).collect::<Vec<_>>()
            );
        }
        for (n, &p) in nodes.iter().zip(pred.iter()) {
            assert_eq!(*n.dht().lock_predecessor()?, Some(dids[p]));
        }
        Ok(())
    }

    async fn test_triple_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        run_triple_stabilization(
            key1,
            key2,
            key3,
            vec![2, 0],
            [vec![1, 2], vec![2, 0], vec![0, 1]],
            [2, 0, 1],
        )
        .await
    }

    async fn test_triple_desc_ordered_nodes_stabilization(
        key1: SecretKey,
        key2: SecretKey,
        key3: SecretKey,
    ) -> Result<()> {
        run_triple_stabilization(
            key1,
            key2,
            key3,
            vec![0, 2],
            [vec![2, 1], vec![0, 2], vec![1, 0]],
            [1, 2, 0],
        )
        .await
    }

    async fn run_stabilization_once(swarm: Arc<Swarm>) -> Result<()> {
        let stab = swarm.stabilizer();
        stab.notify_predecessor().await
    }
}
