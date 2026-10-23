//! Relay custody past nodes without a relay store (`dht::relay_holder`): the owner of an inbox
//! key that holds no relay mail is skipped, the next node along the successor path that holds
//! relay mail keeps the mail, and the recipient gets it on return, from its own store when it
//! keeps one, fetched from the holder when it does not.

use super::drain_and_assert_delivered;
use super::next_held_message;
use super::HELD_MESSAGE;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::ecc::tests::gen_ordered_keys;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::application::ApplicationData;
use crate::tests::default::prepare_node;
use crate::tests::default::prepare_node_without_relay_storage;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_storage_absence;
use crate::tests::default::wait_for_storage_entry;
use crate::tests::default::wait_for_storage_state;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;
use crate::utils::get_epoch_ms;

/// Phase 1: `owner` (no relay store) and `holder` form the ring, and a message from `owner` to
/// `offline` is held. `holder` is responsible for the offline position; the inbox key's owner
/// is `owner`, which holds no relay mail, so `holder`, the next node, keeps the carrier itself.
async fn hold_past_a_storage_less_owner(
    owner: &Node,
    holder: &Node,
    offline: Did,
    network: &[&Node],
) -> Result<()> {
    wait_for_msgs(network.iter().copied()).await;
    wait_for_successor(owner, holder.did()).await?;
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(network.iter().copied()).await;
    wait_for_predecessor(holder, owner.did()).await?;

    owner
        .swarm
        .send_message(ApplicationData::custom(HELD_MESSAGE), offline)
        .await?;
    let held = wait_for_storage_entry(holder, StorageKey::inbox_of(offline)).await?;
    assert_eq!(held.data.len(), 1);
    assert!(!owner.dht().holds_relay_mail());
    assert!(owner
        .dht()
        .live_storage_entry(StorageKey::inbox_of(offline), get_epoch_ms())
        .await?
        .is_none());
    Ok(())
}

/// The peer `returning` joins between `owner` and `holder` through `holder`, and the ring
/// settles: `owner`'s head is `returning`, `holder`'s predecessor is `returning`.
async fn rejoin_between(owner: &Node, returning: &Node, holder: &Node) -> Result<()> {
    manually_establish_connection(&returning.swarm, &holder.swarm).await;
    wait_for_msgs([owner, returning, holder]).await;
    wait_for_successor(returning, holder.did()).await?;
    returning.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([owner, returning, holder]).await;
    wait_for_predecessor(holder, returning.did()).await?;
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([owner, returning, holder]).await;
    wait_for_successor(owner, returning.did()).await?;
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_predecessor(returning, owner.did()).await
}

/// The inbox key's owner keeps no relay store, so the message is held by the next node, which
/// holds relay mail; when the recipient returns with a relay store of its own, it is the holder
/// again: the next node hands the carrier back to it, and it delivers the message.
#[tokio::test]
async fn test_mail_past_a_storage_less_owner_is_held_by_the_next_node_and_delivered() -> Result<()>
{
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node_without_relay_storage(key1)?;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    hold_past_a_storage_less_owner(&node1, &node3, offline, &[&node1, &node3]).await?;

    let node2 = prepare_node(key2).await;
    rejoin_between(&node1, &node2, &node3).await?;
    // node3's repair pass names node2 the holder of `node2 + 1`, and node2 takes the carrier
    // from the next holder along its successor list.
    node3.swarm.stabilizer().stabilize().await?;
    let inbox = StorageKey::inbox_of(node2.did());
    wait_for_storage_entry(&node2, inbox).await?;
    wait_for_storage_absence(&node3, inbox).await?;

    drain_and_assert_delivered(&node2, node1.did()).await
}

/// Neither the inbox key's owner nor the returning recipient keeps a relay store: the carrier
/// stays with the next node that holds relay mail, the recipient fetches its inbox from it,
/// delivers the message, and retires it at the holder.
#[tokio::test]
async fn test_a_storage_less_recipient_fetches_its_inbox_from_its_holder() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node_without_relay_storage(key1)?;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    hold_past_a_storage_less_owner(&node1, &node3, offline, &[&node1, &node3]).await?;

    let node2 = prepare_node_without_relay_storage(key2)?;
    rejoin_between(&node1, &node2, &node3).await?;
    let inbox = StorageKey::inbox_of(node2.did());
    // node2 holds no relay mail: node3 is still the holder, and keeps the carrier.
    node3.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([&node1, &node2, &node3]).await;
    assert!(node3
        .dht()
        .live_storage_entry(inbox, get_epoch_ms())
        .await?
        .is_some());

    let application = node2.swarm.application_inbox()?;
    node2.swarm.stabilizer().stabilize().await?;
    let delivered = next_held_message(&application).await?;
    assert_eq!(delivered.from, node1.did());
    node2.swarm.stabilizer().stabilize().await?;

    let retired = wait_for_storage_state(&node3, inbox, "drained", |stored| {
        stored.is_some_and(|entry| entry.data.is_empty())
    })
    .await?
    .ok_or_else(|| Error::InvalidMessage("drained inbox vanished".to_string()))?;
    assert!(!retired.crdt.tombstones.is_empty());
    Ok(())
}
