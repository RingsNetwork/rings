//! Liveness judges a peer that withholds credit (#913 R7 H1): a probe counts as sent once it is
//! queued, so a probe the peer will not let this end send is unanswered; a credit stall makes a
//! probe due however recently the peer sent anything; probes do not wait on each other; and a
//! forward is released once it is queued, so one peer's backpressure never holds the inbound
//! lane of another.

use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::LaneCreditReservation;

use super::*;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;
use crate::tests::activity::probe_on_activity;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::TEST_NETWORK_ID;

/// The control lane, which every probe rides.
const CONTROL_LANE: ChannelLane = ChannelLane::new(0);

/// Two nodes connected to each other, settled.
async fn connected_pair() -> Result<(Node, Node)> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    Ok((node1, node2))
}

/// Make `withholder` grant `peer` no more credit, then hold at `peer` every credit of `lane`
/// already granted: the lane is starved for good, with no grant still to come.
fn starve(withholder: &Node, peer: &Node, lane: ChannelLane) -> Result<Vec<LaneCreditReservation>> {
    let connection = withholder
        .swarm
        .transport
        .get_connection(peer.did())
        .ok_or(Error::SwarmMissDidInTable(peer.did()))?;
    dummy_controlled::withhold_credit(&connection.dummy_generation_id()?);
    peer.swarm
        .transport
        .hold_lane_credit_for_test(withholder.did(), lane)
}

/// Age `peer` past the liveness idle interval at `node`.
fn age_past_idle(node: &Node, peer: Did) -> Result<()> {
    node.swarm
        .transport
        .force_peer_last_inbound_at(peer, get_epoch_ms_i64() - PEER_LIVENESS_IDLE_MS - 1)
}

/// A peer that withholds the control lane's credit is evicted by liveness: its probe, queued
/// but never sendable, counts as sent from the moment it is queued, and once the answer window
/// passes unanswered the peer is evicted, which fails the probe's credit wait with it.
#[tokio::test]
async fn test_a_peer_withholding_control_credit_is_evicted_by_liveness() -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let peer = node2.did();
    let held = starve(&node2, &node1, CONTROL_LANE)?;
    assert!(
        !held.is_empty(),
        "the peer granted the control lane a window"
    );
    age_past_idle(&node1, peer)?;

    let queued_at = get_epoch_ms_i64();
    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    let unanswered = node1
        .swarm
        .transport
        .peer_liveness_unanswered_since_for_test(peer)?;
    assert!(
        unanswered.is_some_and(|since| since >= queued_at),
        "a probe waiting for credit counts as sent once queued: {unanswered:?}"
    );

    node1.swarm.transport.force_peer_liveness_probe_sent_at(
        peer,
        get_epoch_ms_i64() - PEER_LIVENESS_TIMEOUT_MS - 1,
    )?;
    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;
    assert!(node1.swarm.transport.get_connection(peer).is_none());
    drop(held);
    Ok(())
}

/// A credit stall makes a probe due although the peer is not idle; the probe rides the control
/// lane, so a peer whose control lane progresses answers it and is kept.
#[tokio::test]
async fn test_a_credit_stall_probes_a_busy_peer_that_answers_and_is_kept() -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let peer = node2.did();
    node1
        .swarm
        .transport
        .force_credit_stall_for_test(peer, get_epoch_ms_i64() - PEER_LIVENESS_IDLE_MS - 1);

    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    assert!(
        node1
            .swarm
            .transport
            .peer_liveness_unanswered_since_for_test(peer)?
            .is_some(),
        "the stall made a probe due"
    );
    wait_for_msgs([&node1, &node2]).await;
    assert_eq!(
        node1
            .swarm
            .transport
            .peer_liveness_unanswered_since_for_test(peer)?,
        None,
        "the peer answered on its control lane"
    );
    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;
    assert!(node1.swarm.transport.get_connection(peer).is_some());
    Ok(())
}

/// Probes do not wait on each other: with two peers withholding control credit, the probe pass
/// returns while their probes still wait, and the peer that grants credit is probed and answers.
/// (A starved peer's own traffic may still prove it live; that is liveness, not the probe.)
#[tokio::test]
async fn test_peers_withholding_credit_delay_no_other_probe() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let mut peers = Vec::new();
    for _ in 0..3 {
        let peer = prepare_node(SecretKey::random()).await;
        manually_establish_connection(&node.swarm, &peer.swarm).await;
        peers.push(peer);
    }
    let [starving_a, starving_b, live] = &peers[..] else {
        return Err(Error::InvalidMessage("three peers".to_string()));
    };
    wait_for_msgs([&node, starving_a, starving_b, live]).await;
    let mut held = Vec::new();
    for starving in [starving_a, starving_b] {
        held.extend(starve(starving, &node, CONTROL_LANE)?);
    }
    for peer in &peers {
        age_past_idle(&node, peer.did())?;
    }

    timeout(
        Duration::from_secs(5),
        node.swarm.stabilizer().probe_peer_liveness_for_simulation(),
    )
    .await
    .map_err(|_| Error::InvalidMessage("a probe pass waited on a starved peer".to_string()))??;
    let unanswered = |peer: &Node| {
        node.swarm
            .transport
            .peer_liveness_unanswered_since_for_test(peer.did())
    };
    // The starved probes stay queued, so the node never quiesces: wait for the live answer.
    probe_on_activity("the live peer answers its probe", TEST_HANG_GUARD, || {
        let answered = unanswered(live).map(|since| since.is_none().then_some(()));
        async move { answered }
    })
    .await?;
    // The starved probes still wait in their peers' queues: the pass did not wait for them.
    for starving in [starving_a, starving_b] {
        assert!(node
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(starving.did())
            .is_some_and(|queued| queued >= 1));
    }
    drop(held);
    Ok(())
}

/// A forward is released once it is queued: while its next hop withholds the lane's credit, the
/// send of a payload this node did not originate returns at once with the transfer waiting in
/// the next hop's queue, instead of holding the inbound lane that carried it.
#[tokio::test]
async fn test_a_forward_returns_once_queued_while_its_next_hop_withholds_credit() -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let next_hop = node2.did();
    let held = starve(&node2, &node1, ChannelLane::new(3))?;
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let forwarded = MessagePayload::new_send(
        Message::custom(b"forwarded behind a starved lane")?,
        MessageSigner::new(&origin, TEST_NETWORK_ID),
        next_hop,
        next_hop,
    )?;

    timeout(
        Duration::from_secs(5),
        node1.swarm.transport.send_payload(forwarded),
    )
    .await
    .map_err(|_| Error::InvalidMessage("the forward held its caller".to_string()))??;
    assert_eq!(
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(next_hop),
        Some(1),
        "the forward waits in the next hop's queue, apart from its sender"
    );
    drop(held);
    Ok(())
}
