//! Replicated relay custody (#777; `Law (Chain replication)`, `Law (Chain repair)`, `Law (Age
//! gate)`): a hold is acknowledged to its writer only once every member of its key holds it, mail
//! survives the loss of its holder, read by the recipient from a surviving member, a pass offers
//! one digest set per chain neighbour, and a young node takes no custody.

use rings_transport::core::transport::WebrtcConnectionState;

use super::next_held_message;
use super::HELD_MESSAGE;
use crate::consts::DEFAULT_RELAY_INBOX_REPLICAS;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::Did;
use crate::dht::RelayCustody;
use crate::dht::RelayReplication;
use crate::dht::StorageKey;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::lifecycle::StopSource;
use crate::message::Message;
use crate::message::ProbeRequest;
use crate::swarm::application::ApplicationData;
use crate::swarm::transport::ProbeScope;
use crate::swarm::transport::ProbeWork;
use crate::swarm::transport::CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS;
use crate::swarm::transport::CHAIN_NEIGHBOUR_SUSPICION_MS;
use crate::tests::default::gen_pure_dht;
use crate::tests::default::prepare_node;
use crate::tests::default::prepare_node_without_relay_storage;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_storage_entry;
use crate::tests::default::wait_for_storage_state;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::manually_establish_connection;
use crate::utils::get_epoch_ms;
use crate::utils::get_epoch_ms_i64;

/// The ring `first → second → third → first` of three nodes that hold relay mail.
async fn ring_of_three(first: &Node, second: &Node, third: &Node) -> Result<()> {
    let ring = [first, second, third];
    manually_establish_connection(&first.swarm, &second.swarm).await;
    wait_for_msgs(ring).await;
    wait_for_successor(first, second.did()).await?;
    first.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(ring).await;
    wait_for_predecessor(second, first.did()).await?;

    // `third` joins between `second` and `first`, through `first`, its successor.
    manually_establish_connection(&third.swarm, &first.swarm).await;
    wait_for_msgs(ring).await;
    wait_for_successor(third, first.did()).await?;
    third.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(ring).await;
    wait_for_predecessor(first, third.did()).await?;
    second.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(ring).await;
    wait_for_successor(second, third.did()).await?;
    second.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(ring).await;
    wait_for_predecessor(third, second.did()).await
}

/// Repair passes of every node of `ring`, enough for the previous-holder lists to settle: a
/// list crosses one link per pass and reaches its r-th holder through at most r runs (`Law
/// (Previous holders)`), so r passes per node bound it. Each round waits for the messages of
/// `network`, which also counts a departed node whose last messages are still in flight.
async fn settle_previous_holders(ring: &[&Node], network: &[&Node]) -> Result<()> {
    for _ in 0..DEFAULT_RELAY_INBOX_REPLICAS * ring.len() {
        for node in ring {
            node.swarm.stabilizer().stabilize().await?;
        }
        wait_for_msgs(network.iter().copied()).await;
    }
    Ok(())
}

/// Every node of `nodes` holds one element in the carrier of `inbox`.
async fn assert_every_member_holds(nodes: &[&Node], inbox: StorageKey) -> Result<()> {
    for node in nodes {
        let held = wait_for_storage_entry(node, inbox).await?;
        assert_eq!(held.data.len(), 1, "{} holds {held:?}", node.did());
    }
    Ok(())
}

/// The writer of a hold is acknowledged only once every member of the key holds it: the holder
/// forwards it to the other members, which acknowledge it, and only then the writer forgets it.
#[tokio::test]
async fn test_a_hold_is_acknowledged_once_every_member_holds_it() -> Result<()> {
    let [key1, key2, key3, key4] = gen_ordered_keys::<4>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let node4 = prepare_node(key4).await;
    let offline: Did = key2.address().into();
    ring_of_three(&node1, &node3, &node4).await?;
    let ring = [&node1, &node3, &node4];
    settle_previous_holders(&ring, &ring).await?;
    for node in ring {
        assert!(node.dht().is_relay_member(inbox_key(offline))?);
    }

    // node3, responsible for `offline`, writes the hold; node1, the owner of `offline + 1`, is
    // its holder and forwards it to node3 and node4.
    node1
        .swarm
        .send_message(ApplicationData::custom(HELD_MESSAGE), offline)
        .await?;
    wait_for_msgs(ring).await;
    assert_every_member_holds(&ring, StorageKey::inbox_of(offline)).await?;
    assert!(node3.dht().unacknowledged_relay_holds()?.is_empty());
    Ok(())
}

/// A held message survives the loss of its holder: the recipient returns without a relay store,
/// reads its inbox from a surviving member, takes the message and removes it there.
#[tokio::test]
async fn test_mail_survives_the_loss_of_its_holder() -> Result<()> {
    let [key1, key2, key3, key4] = gen_ordered_keys::<4>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let node4 = prepare_node(key4).await;
    let recipient: Did = key2.address().into();
    ring_of_three(&node1, &node3, &node4).await?;
    let ring = [&node1, &node3, &node4];
    settle_previous_holders(&ring, &ring).await?;
    node1
        .swarm
        .send_message(ApplicationData::custom(HELD_MESSAGE), recipient)
        .await?;
    wait_for_msgs(ring).await;
    let inbox = StorageKey::inbox_of(recipient);
    assert_every_member_holds(&ring, inbox).await?;

    // node1, the holder, leaves: node3 and node4 retire it.
    for survivor in [&node3, &node4] {
        survivor
            .swarm
            .transport
            .force_peer_connection_state_without_callback(
                node1.did(),
                WebrtcConnectionState::Closed,
            )?;
        survivor
            .swarm
            .stabilizer()
            .clean_unavailable_connections()
            .await?;
    }
    node1.swarm.disconnect(node3.did()).await?;
    node1.swarm.disconnect(node4.did()).await?;
    // node1's last messages are still counted until they arrive, so it stays in every wait.
    wait_for_msgs(ring).await;
    settle_previous_holders(&[&node3, &node4], &ring).await?;

    // The recipient returns without a relay store, between node4 and node3, through node3.
    let node2 = prepare_node_without_relay_storage(key2)?;
    let network = [&node1, &node2, &node3, &node4];
    manually_establish_connection(&node2.swarm, &node3.swarm).await;
    wait_for_msgs(network).await;
    wait_for_successor(&node2, node3.did()).await?;
    settle_previous_holders(&[&node2, &node3, &node4], &network).await?;

    let application = node2.swarm.application_inbox()?;
    node2.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(network).await;
    let delivered = next_held_message(&application).await?;
    assert_eq!(delivered.from, node1.did());
    node2.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(network).await;
    wait_for_storage_state(
        &node3,
        inbox,
        "removed at the member it was read from",
        |stored| stored.is_some_and(|entry| entry.data.is_empty()),
    )
    .await
    .map(drop)
}

/// The age gate (`Law (Age gate)`): a node with a relay store declares that it holds relay mail
/// only once it has been online for the gate, and then to each connected peer once: a peer it
/// declared to is not declared to again, and a peer it lost is declared to again on return.
#[test]
fn test_the_age_gate_closes_custody_and_declares_it_once_per_peer() -> Result<()> {
    let did: Did = SecretKey::random().address().into();
    let [first, second]: [Did; 2] = [
        SecretKey::random().address().into(),
        SecretKey::random().address().into(),
    ];
    let gated = gen_pure_dht(did)
        .with_relay_replication(RelayReplication {
            age_gate_ms: u64::MAX,
            ..RelayReplication::default()
        })
        .with_relay_storage(crate::tests::volatile_relay_storage());
    assert!(!gated.holds_relay_mail());
    assert_eq!(gated.relay_custody(), RelayCustody::HoldsNone);
    assert!(gated
        .undeclared_relay_custody_peers(vec![first, second])?
        .is_empty());

    let open = gen_pure_dht(did)
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(crate::tests::volatile_relay_storage());
    assert_eq!(open.relay_custody(), RelayCustody::Holds);
    assert_eq!(open.undeclared_relay_custody_peers(vec![first, second])?, [
        first, second
    ]);
    // Only the declaration to `first` was queued: `second` alone is declared to again.
    open.record_relay_custody_declared(first)?;
    assert_eq!(open.undeclared_relay_custody_peers(vec![first, second])?, [
        second
    ]);
    // A peer that was lost is declared to again when it returns.
    open.forget_relay_custody(first)?;
    assert_eq!(open.undeclared_relay_custody_peers(vec![first])?, [first]);
    Ok(())
}

/// Chain repair is coalesced per neighbour (`Law (Chain repair)`, Prop. 13 of durable-records):
/// however many carriers a member keeps, a pass offers one digest set to each of at most two
/// chain neighbours, and every carrier it keeps is in an offer.
#[tokio::test]
async fn test_a_repair_pass_offers_one_digest_set_per_chain_neighbour() -> Result<()> {
    let [key1, key2, key3, key4, key5, key6] = gen_ordered_keys::<6>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let node5 = prepare_node(key5).await;
    ring_of_three(&node1, &node3, &node5).await?;
    let ring = [&node1, &node3, &node5];
    settle_previous_holders(&ring, &ring).await?;
    for offline in [key2, key4, key6] {
        node1
            .swarm
            .send_message(
                ApplicationData::custom(HELD_MESSAGE),
                offline.address().into(),
            )
            .await?;
    }
    wait_for_msgs(ring).await;
    for node in ring {
        let offers = node.dht().relay_repair_offers(get_epoch_ms()).await?;
        assert!(offers.len() <= 2, "{} offers to {offers:?}", node.did());
        let offered = offers
            .values()
            .flatten()
            .map(|carrier| carrier.key)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            offered.len(),
            3,
            "{} offers every carrier it keeps",
            node.did()
        );
    }
    Ok(())
}

/// A chain neighbour's loss runs the chain exchange at once (`Law (Event-driven repair)`): with
/// no maintenance loop running at all, so no periodic pass that could do it, the holder that
/// retires its next member offers its digest set to its new chain neighbour.
#[tokio::test]
async fn test_a_chain_neighbours_loss_runs_the_chain_exchange_at_once() -> Result<()> {
    let [key1, key2, key3, key4] = gen_ordered_keys::<4>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let node4 = prepare_node(key4).await;
    ring_of_three(&node1, &node3, &node4).await?;
    let ring = [&node1, &node3, &node4];
    settle_previous_holders(&ring, &ring).await?;
    node1
        .swarm
        .send_message(ApplicationData::custom(HELD_MESSAGE), key2.address().into())
        .await?;
    wait_for_msgs(ring).await;
    assert_every_member_holds(&ring, StorageKey::inbox_of(key2.address().into())).await?;
    while node4.try_listen_once().await.is_some() {}

    // Only the chain task runs on node1: the event alone can start the exchange.
    let stop = StopSource::new();
    let token = stop.token();
    let stabilizer = node1.swarm.stabilizer();
    let chain = tokio::spawn(async move { stabilizer.repair_relay_chain_on_loss(&token).await });

    // node1 retires node3, its next member; node4 becomes its next chain neighbour.
    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(node3.did(), WebrtcConnectionState::Closed)?;
    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    let offered = tokio::time::timeout(TEST_HANG_GUARD, async {
        let mut scan = node4.message_scan().await;
        while let Some(payload) = scan.next().await {
            let offer = payload.transaction.data::<Message>()?;
            if matches!(offer, Message::RelayCarrierDigests(_))
                && payload.transaction.origin() == node1.did()
            {
                return Ok::<bool, crate::error::Error>(true);
            }
        }
        Ok(false)
    })
    .await
    .unwrap_or(Ok(false))?;
    stop.request_stop();
    chain
        .await
        .map_err(|error| crate::error::Error::InvalidMessage(error.to_string()))?;
    assert!(offered, "node1 offered nothing to node4 after losing node3");
    Ok(())
}

/// Suspect node1's next chain neighbour `peer` on a probe issued the chain timeout ago, and
/// return the probe's challenge.
async fn suspect_by_a_late_probe(node1: &Node, peer: Did) -> Result<ProbeRequest> {
    let request = node1.swarm.transport.force_peer_liveness_probe_issued_at(
        peer,
        get_epoch_ms_i64() - CHAIN_NEIGHBOUR_SUSPICION_MS - 1,
    )?;
    node1
        .swarm
        .stabilizer()
        .probe_relay_chain_neighbours()
        .await?;
    Ok(request)
}

/// Answer `peer`'s outstanding probe of `request` at node1.
fn answer(node1: &Node, peer: Did, request: ProbeRequest) -> Result<()> {
    assert!(node1.swarm.transport.answer_liveness_probe(peer, request)?);
    Ok(())
}

/// Give node1 an on-time answer from `peer`: a probe issued now, answered at once.
fn answer_on_time(node1: &Node, peer: Did) -> Result<()> {
    let request = node1
        .swarm
        .transport
        .force_peer_liveness_probe_issued_at(peer, get_epoch_ms_i64())?;
    answer(node1, peer, request)
}

/// A ring of three holders whose views have settled, node1 first.
async fn settled_ring() -> Result<(Node, Node, Node)> {
    let [key1, key3, key4] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let node4 = prepare_node(key4).await;
    ring_of_three(&node1, &node3, &node4).await?;
    let ring = [&node1, &node3, &node4];
    settle_previous_holders(&ring, &ring).await?;
    Ok((node1, node3, node4))
}

/// A chain neighbour whose probe goes unanswered for the chain timeout is suspected for relay
/// custody (#1070): the chain moves past it and the chain exchange is due at once, but it is not
/// evicted; two on-time answers in a row clear the suspicion, which is again a chain change.
#[tokio::test]
async fn test_an_unanswering_chain_neighbour_is_suspected_and_cleared_without_eviction(
) -> Result<()> {
    let (node1, node3, node4) = settled_ring().await?;
    assert_eq!(node1.dht().relay_chain_neighbours()?.1, Some(node3.did()));
    node1.dht().take_relay_chain_repair_due();

    let late = suspect_by_a_late_probe(&node1, node3.did()).await?;
    assert!(node1.dht().is_relay_suspected(node3.did())?);
    assert!(
        node1.dht().take_relay_chain_repair_due(),
        "the suspicion is a chain change"
    );
    assert_eq!(node1.dht().relay_chain_neighbours()?.1, Some(node4.did()));
    assert!(
        node1.swarm.transport.has_active_connection(node3.did()),
        "suspicion evicts nothing"
    );

    // The late answer does not clear it; two on-time answers in a row do.
    answer(&node1, node3.did(), late)?;
    answer_on_time(&node1, node3.did())?;
    assert!(node1.dht().is_relay_suspected(node3.did())?);
    assert!(!node1.dht().take_relay_chain_repair_due());
    answer_on_time(&node1, node3.did())?;
    assert!(!node1.dht().is_relay_suspected(node3.did())?);
    assert!(
        node1.dht().take_relay_chain_repair_due(),
        "the clearing is a chain change"
    );
    assert_eq!(node1.dht().relay_chain_neighbours()?.1, Some(node3.did()));
    Ok(())
}

/// A neighbour whose answers alternate late and on time never clears, so it changes the chains
/// once, not once per period (`Law (Clearing)`): the chain changes per neighbour stay within one
/// suspicion and one clearing per `τ_c + k·P_c`.
#[tokio::test]
async fn test_a_flapping_chain_neighbour_changes_the_chains_once() -> Result<()> {
    let (node1, node3, _node4) = settled_ring().await?;
    let before = node1.dht().relay_chain_changes_for_test();
    for _ in 0..5 {
        let late = suspect_by_a_late_probe(&node1, node3.did()).await?;
        answer(&node1, node3.did(), late)?;
        answer_on_time(&node1, node3.did())?;
    }
    assert!(node1.dht().is_relay_suspected(node3.did())?);
    assert_eq!(
        node1.dht().relay_chain_changes_for_test() - before,
        1,
        "five late/on-time cycles are one suspicion"
    );
    answer_on_time(&node1, node3.did())?;
    assert!(!node1.dht().is_relay_suspected(node3.did())?);
    assert_eq!(node1.dht().relay_chain_changes_for_test() - before, 2);
    Ok(())
}

/// A suspect stays on the chain period (`Law (Clearing)`): the chains skip it, but the chain
/// probe pass still issues its probe a chain period after the last, so the on-time answers that
/// clear it come on the schedule the clearing bound counts.
#[tokio::test]
async fn test_a_suspect_is_still_probed_on_the_chain_period() -> Result<()> {
    let (node1, node3, node4) = settled_ring().await?;
    let late = suspect_by_a_late_probe(&node1, node3.did()).await?;
    answer(&node1, node3.did(), late)?;
    assert!(node1.dht().is_relay_suspected(node3.did())?);
    let (previous, next) = node1.dht().relay_chain_neighbours()?;
    assert!(previous != Some(node3.did()) && next == Some(node4.did()));

    let transport = &node1.swarm.transport;
    transport
        .restart_peer_liveness_period_for_test(node3.did(), CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS)?;
    let pass = transport
        .liveness_probe_pass(get_epoch_ms_i64(), ProbeScope::ChainNeighbours)?
        .ok_or_else(|| crate::error::Error::InvalidMessage("probe pass busy".to_string()))?;
    assert!(
        pass.work().iter().any(|work| matches!(
            work,
            ProbeWork::Issue(attempt) if attempt.peer() == node3.did()
        )),
        "the suspect's probe is due on the chain period"
    );
    Ok(())
}
