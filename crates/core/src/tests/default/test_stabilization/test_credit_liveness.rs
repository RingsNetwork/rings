//! Liveness judges a peer by its probes alone (L3, `swarm::transport::liveness`): a probe's
//! deadline counts from its issue, so a probe the peer will not let this end send goes
//! unanswered (#913 R7 H1); the peer's own traffic clears nothing (#913 R8 M1); a peer slow
//! only on a data lane answers on lane 0 and is kept (#913 R6 H1); a probe takes no shared
//! outbound capacity, so other traffic cannot refuse it (#913 R8 M2); and no probe waits on
//! its peer.
//! Every send a handler makes is released once it is queued (#913 R8 H1, the inbound-locality
//! law of `swarm::transport::egress`), so one peer's backpressure never holds the inbound lane
//! of another; a send through the Swarm API still waits for its first frame's admission.

use rings_transport::core::pool::ChannelLane;
use tokio::time::timeout;
use tokio::time::Duration;

use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::swarm::transport::PEER_LIVENESS_TIMEOUT_MS;
use crate::tests::activity::probe_on_activity;
use crate::tests::default::connected_nodes;
use crate::tests::default::credit_starvation::starve;
use crate::tests::default::credit_starvation::starve_every_lane;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::manually_establish_connection;
use crate::utils::get_epoch_ms_i64;

/// The control lane, which every probe rides.
const CONTROL_LANE: ChannelLane = ChannelLane::new(0);
/// The application data lane.
const APPLICATION_LANE: ChannelLane = ChannelLane::new(3);

/// A peer that stopped granting credit and starved its application lane is still probed on
/// its period, answers on its control lane's window, and is kept: a data-lane stall is
/// backpressure, never a verdict.
#[tokio::test]
async fn test_a_peer_slow_on_a_data_lane_answers_its_probe_and_is_kept() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let held = starve(&node2, &node1, APPLICATION_LANE)?;
    node1.swarm.transport.force_peer_liveness_probe_due(peer)?;

    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    // Issuing restarts the period, answered or not, so this holds whatever ran first.
    assert!(
        !node1
            .swarm
            .transport
            .peer_liveness_probe_due_for_test(peer)?,
        "the probe was issued"
    );
    wait_for_msgs([&node1, &node2]).await;
    assert_eq!(
        node1
            .swarm
            .transport
            .peer_liveness_outstanding_since_for_test(peer)?,
        None,
        "the peer answered on its control lane"
    );
    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;
    assert!(node1.swarm.transport.get_connection(peer).is_some());
    drop(held);
    Ok(())
}

/// A peer that withholds control credit cannot clear its outstanding probe by sending: only
/// the probe's answer clears it.
#[tokio::test]
async fn test_a_withholding_peers_own_traffic_leaves_its_probe_outstanding() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let held = starve(&node2, &node1, CONTROL_LANE)?;
    node1.swarm.transport.force_peer_liveness_probe_due(peer)?;
    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    let issued = node1
        .swarm
        .transport
        .peer_liveness_outstanding_since_for_test(peer)?;
    assert!(issued.is_some(), "the probe was issued");

    node2
        .swarm
        .send_direct_message(Message::custom(b"still sending")?, node1.did())
        .await?;
    timeout(TEST_HANG_GUARD, async {
        while let Some(payload) = node1.listen_once().await {
            if matches!(payload.transaction.data(), Ok(Message::CustomMessage(_))) {
                return;
            }
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("the peer's message never arrived".to_string()))?;
    assert_eq!(
        node1
            .swarm
            .transport
            .peer_liveness_outstanding_since_for_test(peer)?,
        issued,
        "traffic cleared nothing"
    );
    drop(held);
    Ok(())
}

/// A bound on one probe pass, well inside a probe's deadline: a pass that waited for a starved
/// probe's admission would return only when that probe's transfer expires, at its deadline from
/// issue (#913 R10 M5), so it cannot meet the bound.
const PROBE_PASS_BOUND: Duration = Duration::from_secs(15);
const _: () = assert!((PROBE_PASS_BOUND.as_millis() as i64) < PEER_LIVENESS_TIMEOUT_MS);

/// Probes do not wait on their peers: with two peers withholding control credit, the probe pass
/// returns while their probes are provably queued, and the peer that grants credit is probed
/// and answers (#913 R10 M9).
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
        node.swarm
            .transport
            .force_peer_liveness_probe_due(peer.did())?;
    }

    timeout(
        PROBE_PASS_BOUND,
        node.swarm.stabilizer().probe_peer_liveness_for_simulation(),
    )
    .await
    .map_err(|_| Error::InvalidMessage("a probe pass waited on a starved peer".to_string()))??;
    // The pass returned while both starved probes sit in their queues, waiting for credit.
    for starving in [starving_a, starving_b] {
        assert!(
            node.swarm
                .transport
                .peer_liveness_probe_queued_for_test(starving.did())?,
            "the starved probe is outstanding and queued"
        );
    }
    // The starved probes stay queued, so the node never quiesces: wait for the live answer.
    probe_on_activity("the live peer answers its probe", TEST_HANG_GUARD, || {
        let answered = node
            .swarm
            .transport
            .peer_liveness_outstanding_since_for_test(live.did())
            .map(|since| since.is_none().then_some(()));
        async move { answered }
    })
    .await?;
    drop(held);
    Ok(())
}

/// A probe takes no shared outbound capacity (#913 R8 M2): with the control capacity toward a
/// peer that withholds control credit held full by other transfers, the probe is still queued
/// and its deadline runs.
#[tokio::test]
async fn test_a_probe_is_queued_although_the_peers_outbound_capacity_is_full() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let credit = starve(&node2, &node1, CONTROL_LANE)?;
    let capacity = node1.swarm.transport.hold_control_capacity_for_test(peer)?;
    node1.swarm.transport.force_peer_liveness_probe_due(peer)?;

    let issued_at = get_epoch_ms_i64();
    node1
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    assert!(
        node1
            .swarm
            .transport
            .peer_liveness_outstanding_since_for_test(peer)?
            .is_some_and(|since| since >= issued_at),
        "the probe was issued and queued despite the full capacity"
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
    let (node1, node2) = connected_nodes().await?;
    let held = starve_every_lane(&node2, &node1)?;

    let sent = timeout(
        TEST_HANG_GUARD,
        node1
            .swarm
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

/// No idle credit: once a link is quiet, the sending end's outbound worker holds none of the
/// control lane's credit, so the whole window is free, and a peer that stops granting credit
/// stops this end's control traffic outright.
#[tokio::test]
async fn test_a_quiet_link_holds_no_credit_at_its_sender() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let free = node1
        .swarm
        .transport
        .hold_lane_credit_for_test(node2.did(), CONTROL_LANE)?;
    assert_eq!(
        u64::try_from(free.len()).map_err(|_| Error::InvalidMessage("window".to_string()))?,
        rings_transport::core::credit::LANE_CREDIT_WINDOW
    );
    Ok(())
}

/// The three-node cascade (#913 R7, R8): `M` withholds every credit from `B`, and `X` relays
/// through `B` a control message whose next hop is `M`. `B` queues the forward toward `M` and
/// frees the lane-0 event that carried it, so `X`'s lane 0 at `B` keeps progressing: `X`'s
/// probe of `B`, behind the relayed message on the same lane, is answered, and `X` keeps `B`.
#[tokio::test]
async fn test_a_peer_withholding_credit_from_a_relay_never_costs_the_relay_its_other_links(
) -> Result<()> {
    let (x, b) = connected_nodes().await?;
    let m = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&b.swarm, &m.swarm).await;
    wait_for_msgs([&x, &b, &m]).await;
    let held = starve_every_lane(&m, &b)?;
    // B's forward toward M stays stuck for the whole witness, not only until the short dummy
    // first-frame deadline, so a forward that held its lane would hold it throughout.
    let _stuck = crate::swarm::transport::hold_first_frame_deadlines_for_test();

    // A report B forwards toward M on its control lane, as it would a relayed request's.
    let relayed = Message::FindSuccessorReport(crate::message::FindSuccessorReport {
        did: m.did(),
        handler: crate::message::FindSuccessorReportHandler::None,
    });
    x.swarm
        .transport
        .protocol_egress()
        .send_message_by_hop(relayed, m.did(), b.did())
        .await?;
    x.swarm.transport.force_peer_liveness_probe_due(b.did())?;
    x.swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    // M's starved queue keeps B from quiescing: wait for X's answer itself.
    probe_on_activity("B answers X's probe", TEST_HANG_GUARD, || {
        let answered = x
            .swarm
            .transport
            .peer_liveness_outstanding_since_for_test(b.did())
            .map(|since| since.is_none().then_some(()));
        async move { answered }
    })
    .await?;
    x.swarm.stabilizer().clean_unavailable_connections().await?;
    assert!(x.swarm.transport.get_connection(b.did()).is_some());
    drop(held);
    Ok(())
}
