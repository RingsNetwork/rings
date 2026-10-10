//! Liveness judges a peer that withholds credit (#913 R7 H1): a probe counts as sent once it is
//! queued, so a probe the peer will not let this end send is unanswered; a credit stall makes a
//! probe due however recently the peer sent anything; a probe refused before the queue behind
//! withheld credit is charged (#913 R8 M2); and no probe waits on its peer.
//! Every send a handler makes is released once it is queued (#913 R8 H1, the inbound-locality
//! law of `swarm::transport::egress`), so one peer's backpressure never holds the inbound lane
//! of another; a send through the Swarm API still waits for its first frame's admission.

use rings_transport::core::pool::ChannelLane;

use super::*;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::tests::activity::probe_on_activity;
use crate::tests::default::credit_starvation::connected_pair;
use crate::tests::default::credit_starvation::starve;
use crate::tests::default::credit_starvation::starve_every_lane;
use crate::tests::default::TEST_HANG_GUARD;

/// The control lane, which every probe rides.
const CONTROL_LANE: ChannelLane = ChannelLane::new(0);

/// Age `peer` past the liveness idle interval at `node`.
fn age_past_idle(node: &Node, peer: Did) -> Result<()> {
    node.swarm
        .transport
        .force_peer_last_inbound_at(peer, get_epoch_ms_i64() - PEER_LIVENESS_IDLE_MS - 1)
}

/// A peer that withholds the control lane's credit is evicted by liveness: its probe, queued
/// but never sendable, counts as sent from the moment it is queued, and once the answer window
/// passes unanswered the peer is evicted.
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

/// Probes do not wait on their peers: with two peers withholding control credit, the probe pass
/// returns while their probes still wait in their queues, and the peer that grants credit is
/// probed and answers.
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

/// A probe that cannot even be queued is charged as sent while this end waits for the peer's
/// credit (#913 R8 M2): the capacity it lacks is held by the peer's own backpressure, so a peer
/// that withholds credit cannot keep its probe unsent and itself unjudged. Without a stall the
/// same refusal is this end's failure and charges the peer nothing.
#[tokio::test]
async fn test_a_probe_refused_capacity_is_charged_only_behind_withheld_credit() -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let peer = node2.did();
    let credit = starve(&node2, &node1, CONTROL_LANE)?;
    let capacity = node1.swarm.transport.hold_control_capacity_for_test(peer)?;
    age_past_idle(&node1, peer)?;
    let unanswered = || {
        node1
            .swarm
            .transport
            .peer_liveness_unanswered_since_for_test(peer)
    };

    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    assert_eq!(
        unanswered()?,
        None,
        "a refusal while credit flows charges nothing"
    );

    node1
        .swarm
        .transport
        .force_credit_stall_for_test(peer, get_epoch_ms_i64() - PEER_LIVENESS_IDLE_MS - 1);
    let refused_at = get_epoch_ms_i64();
    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    assert!(
        unanswered()?.is_some_and(|since| since >= refused_at),
        "a probe refused behind withheld credit counts as unanswered"
    );
    drop((capacity, credit));
    Ok(())
}

/// The application discipline is unchanged: a send through the Swarm API waits for its first
/// frame's admission, so while the peer withholds credit it fails at that deadline and the
/// caller learns the message did not leave.
#[tokio::test]
async fn test_an_application_send_waits_for_its_first_frame_while_the_peer_withholds_credit(
) -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let held = starve_every_lane(&node2, &node1)?;

    let sent = timeout(
        TEST_HANG_GUARD,
        node1
            .swarm
            .transport
            .send_direct_message(Message::custom(b"from the application")?, node2.did()),
    )
    .await
    .map_err(|_| Error::InvalidMessage("the application send hung".to_string()))?;
    assert!(
        matches!(sent, Err(Error::OutboundFirstFrameAdmissionTimeout { .. })),
        "the application send waited for admission and timed out: {sent:?}"
    );
    drop(held);
    Ok(())
}
