//! End-to-end law of the relay inbox: a message to an offline peer is held in the peer's inbox
//! carrier by whichever node owns it, handed to the peer with its storage interval when the peer
//! returns, delivered to the peer's application on stabilization, and compacted afterwards.
//!
//! The hand-off is the owner's stabilization placement invariant, so it holds however the owner
//! learns of the returning peer: through its successor's notify report, or through a direct
//! connection from the peer.

use std::time::Instant;

use tokio::time::sleep;

use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::Did;
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
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::default::TEST_WAIT_POLL_INTERVAL;
use crate::tests::default::TEST_WAIT_TIMEOUT;
use crate::tests::manually_establish_connection;

const HELD_MESSAGE: &[u8] = b"held while offline";

fn is_held_message(payload: &MessagePayload) -> bool {
    matches!(
        payload.transaction.data::<Message>(),
        Ok(Message::CustomMessage(message)) if message.0 == HELD_MESSAGE
    )
}

async fn next_held_message(node: &Node) -> Result<MessagePayload> {
    let deadline = Instant::now() + TEST_WAIT_TIMEOUT;
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

async fn wait_for_inbox_compaction(node: &Node, key: Did) -> Result<Entry> {
    let started = Instant::now();
    loop {
        if let Some(entry) = node.dht().storage.get(&key.to_string()).await? {
            if entry.data.is_empty() {
                return Ok(entry);
            }
        }
        assert!(
            started.elapsed() <= TEST_WAIT_TIMEOUT,
            "inbox at {key} was not compacted within {TEST_WAIT_TIMEOUT:?}"
        );
        tokio::task::yield_now().await;
        sleep(TEST_WAIT_POLL_INTERVAL).await;
    }
}

/// Phase 1: node1 and node3 form the ring and a message to the absent `offline` is held.
///
/// node1 routes the message to succ(offline) = node3, which is responsible for the offline
/// position and cannot deliver, so it holds the message in the inbox carrier `offline + 1`.
/// That key lies in node1's storage interval `(node1, node3]`, so the write lands at node1.
async fn hold_message_for_offline_peer(node1: &Node, node3: &Node, offline: Did) -> Result<()> {
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([node1, node3]).await;
    wait_for_successor(node1, node3.did()).await?;

    node1
        .swarm
        .transport
        .send_message(Message::custom(HELD_MESSAGE)?, offline)
        .await?;
    let held = wait_for_storage_entry(node1, inbox_key(offline)).await?;
    assert_eq!(held.kind, EntryKind::RelayMessage);
    assert_eq!(held.data.len(), 1);
    assert!(held
        .deliverable_inbox_messages(node1.swarm.network_id())
        .iter()
        .all(is_held_message));
    Ok(())
}

/// Phase 3: the returned peer's own stabilization round drains its inbox to the application and
/// compacts the delivered message out of the carrier.
async fn assert_inbox_drained(peer: &Node, sender: Did) -> Result<()> {
    let inbox = inbox_key(peer.did());
    peer.swarm.stabilizer()?.stabilize().await?;

    let delivered = next_held_message(peer).await?;
    assert_eq!(delivered.transaction.destination, peer.did());
    assert_eq!(delivered.transaction.signer(), sender);

    let compacted = wait_for_inbox_compaction(peer, inbox).await?;
    assert!(compacted.crdt.register.is_some());
    Ok(())
}

#[tokio::test]
async fn test_message_to_offline_peer_is_held_and_delivered_on_return() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    let inbox = inbox_key(offline);
    hold_message_for_offline_peer(&node1, &node3, offline).await?;

    // The peer returns by joining through its successor node3. node2 notifies node3, node1's
    // next stabilization learns from node3 that node2 now precedes it and connects to it, and the
    // admission moves node1's head to node2. The round after that hands over the inbox carrier,
    // whose key lies in node2's interval `(node2, node3]`.
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node2.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node2, node3.did()).await?;
    node2.swarm.stabilizer()?.stabilize().await?;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_predecessor(&node3, node2.did()).await?;
    node1.swarm.stabilizer()?.stabilize().await?;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node1, node2.did()).await?;
    node1.swarm.stabilizer()?.stabilize().await?;
    wait_for_storage_entry(&node2, inbox).await?;
    wait_for_storage_absence(&node1, inbox).await?;

    assert_inbox_drained(&node2, node1.did()).await
}

#[tokio::test]
async fn test_held_message_is_delivered_when_peer_returns_through_its_predecessor() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    let inbox = inbox_key(offline);
    hold_message_for_offline_peer(&node1, &node3, offline).await?;

    // The peer returns by connecting straight to node1, the inbox owner. No notify report is
    // exchanged: the admission itself moves node1's head to node2, and node1's next round hands
    // the inbox carrier over.
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node2.swarm, &node1.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node1, node2.did()).await?;
    node1.swarm.stabilizer()?.stabilize().await?;
    wait_for_storage_entry(&node2, inbox).await?;
    wait_for_storage_absence(&node1, inbox).await?;

    assert_inbox_drained(&node2, node1.did()).await
}
