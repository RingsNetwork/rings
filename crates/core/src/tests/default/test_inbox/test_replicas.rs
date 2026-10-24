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
use crate::swarm::application::ApplicationData;
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
/// only once it has been online for the gate, and it declares the opening to its peers once.
#[test]
fn test_the_age_gate_closes_custody_and_opens_it_once() {
    let did: Did = SecretKey::random().address().into();
    let gated = gen_pure_dht(did)
        .with_relay_replication(RelayReplication {
            age_gate_ms: u64::MAX,
            ..RelayReplication::default()
        })
        .with_relay_storage(crate::tests::volatile_relay_storage());
    assert!(!gated.holds_relay_mail());
    assert_eq!(gated.relay_custody(), RelayCustody::HoldsNone);
    assert!(!gated.relay_custody_opened());

    let open = gen_pure_dht(did)
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(crate::tests::volatile_relay_storage());
    assert_eq!(open.relay_custody(), RelayCustody::Holds);
    assert!(open.relay_custody_opened());
    assert!(!open.relay_custody_opened(), "the opening is declared once");
    open.declare_relay_custody_again();
    assert!(
        open.relay_custody_opened(),
        "a lost declaration is declared again"
    );
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
