//! End-to-end law of the relay inbox: a message to an offline peer is held in the peer's inbox
//! carrier by whichever node owns it, handed to the peer with its storage interval when the peer
//! returns, delivered to the peer's application on stabilization, and compacted afterwards.
//!
//! The hand-off is the placement invariant of the owner's storage repair pass, so it holds however
//! the owner learns of the returning peer: through its successor's topology report, or through a
//! direct connection from the peer.

mod test_rejoin;

use std::time::Instant;

use crate::dht::entry::EntryKind;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::ecc::tests::gen_ordered_keys;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_storage_absence;
use crate::tests::default::wait_for_storage_entry;
use crate::tests::default::wait_for_storage_state;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::manually_establish_connection;
use crate::utils::get_epoch_ms;

const HELD_MESSAGE: &[u8] = b"held while offline";

fn is_held_message(payload: &MessagePayload) -> bool {
    matches!(
        payload.transaction.data::<Message>(),
        Ok(Message::CustomMessage(message)) if message.0 == HELD_MESSAGE
    )
}

async fn next_held_message(node: &Node) -> Result<MessagePayload> {
    let deadline = Instant::now() + TEST_HANG_GUARD;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::InvalidMessage(
                "held message was not delivered from the inbox".to_string(),
            ));
        }
        match tokio::time::timeout(remaining, node.listen_once()).await {
            Ok(Some(payload)) if is_held_message(&payload) => return Ok(payload),
            Ok(Some(_)) => continue,
            Ok(None) => {
                return Err(Error::InvalidMessage(
                    "message channel closed before the held message arrived".to_string(),
                ));
            }
            Err(_) => continue,
        }
    }
}

/// Phase 1: node1 and node3 already form the ring and a message to `offline` is held.
///
/// node1 routes the message to succ(offline) = node3, which is responsible for the offline
/// position and cannot deliver, so it holds the message in the inbox carrier `offline + 1`.
/// That key lies in node1's storage interval `(node1, node3]`, so the write lands at node1.
///
/// `network` is every node of the test, `node1` and `node3` included: quiescence is decided by
/// frame conservation over all nodes that exchange frames, and a departed peer that still exists
/// may be reconnected by admission's successor synchronization while the message is held.
async fn hold_message_for_offline_peer(
    node1: &Node,
    node3: &Node,
    offline: Did,
    network: &[&Node],
) -> Result<()> {
    wait_for_msgs(network.iter().copied()).await;
    wait_for_successor(node1, node3.did()).await?;
    // node3 is responsible for `offline` only once it knows node1 as its predecessor.
    node1.swarm.stabilizer().stabilize().await?;
    wait_for_msgs(network.iter().copied()).await;
    wait_for_predecessor(node3, node1.did()).await?;

    node1
        .swarm
        .transport
        .send_message(Message::custom(HELD_MESSAGE)?, offline)
        .await?;
    let held = wait_for_storage_entry(node1, StorageKey::inbox_of(offline)).await?;
    assert_eq!(held.kind, EntryKind::RelayMessage);
    assert_eq!(held.data.len(), 1);
    let drain = held.partition_inbox(get_epoch_ms(), node1.swarm.network_id());
    assert!(drain.rejected.crdt.dots.is_empty());
    assert!(drain
        .deliverable
        .iter()
        .all(|element| is_held_message(&element.payload)));
    Ok(())
}

/// Phase 2: the owner's storage repair pass hands the inbox carrier to the returned peer. The
/// peer accepts a relay carrier only from its predecessor, so the owner's notify must have
/// reached it first; the acknowledgement then removes the owner's copy.
async fn hand_off_inbox(owner: &Node, peer: &Node) -> Result<()> {
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_predecessor(peer, owner.did()).await?;
    owner.swarm.stabilizer().stabilize().await?;
    let inbox = StorageKey::inbox_of(peer.did());
    wait_for_storage_entry(peer, inbox).await?;
    wait_for_storage_absence(owner, inbox).await
}

/// Phase 3: the returned peer's own stabilization round drains its inbox to the application and
/// retires the delivered message from the carrier by its dot.
async fn drain_and_assert_delivered(peer: &Node, sender: Did) -> Result<()> {
    let inbox = StorageKey::inbox_of(peer.did());
    peer.swarm.stabilizer().stabilize().await?;

    let delivered = next_held_message(peer).await?;
    assert_eq!(delivered.transaction.destination, peer.did());
    assert_eq!(delivered.transaction.signer(), sender);

    let retired = wait_for_storage_state(peer, inbox, "drained", |stored| {
        stored.is_some_and(|entry| entry.data.is_empty())
    })
    .await?
    .ok_or_else(|| Error::InvalidMessage("drained inbox vanished".to_string()))?;
    assert!(retired.crdt.register.is_none());
    assert!(!retired.crdt.tombstones.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_message_to_offline_peer_is_held_and_delivered_on_return() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    hold_message_for_offline_peer(&node1, &node3, offline, &[&node1, &node3]).await?;

    // The peer returns by joining through its successor node3. node2 notifies node3, node1's
    // next stabilization learns from node3 that node2 now precedes it and connects to it, and the
    // admission moves node1's head to node2. The repair pass then hands over the inbox carrier,
    // whose key lies in node2's interval `(node2, node3]`.
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node2.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node2, node3.did()).await?;
    node2.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_predecessor(&node3, node2.did()).await?;
    node1.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node1, node2.did()).await?;
    hand_off_inbox(&node1, &node2).await?;

    drain_and_assert_delivered(&node2, node1.did()).await
}

#[tokio::test]
async fn test_held_message_is_delivered_when_peer_returns_through_its_predecessor() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    hold_message_for_offline_peer(&node1, &node3, offline, &[&node1, &node3]).await?;

    // The peer returns by connecting straight to node1, the inbox owner. No notify report is
    // exchanged: the admission itself moves node1's head to node2 and requests the repair pass
    // that hands the inbox carrier over.
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node2.swarm, &node1.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node1, node2.did()).await?;
    assert!(node1.swarm.transport.storage_repair_requested());
    hand_off_inbox(&node1, &node2).await?;

    drain_and_assert_delivered(&node2, node1.did()).await
}
