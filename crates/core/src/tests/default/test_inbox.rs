//! End-to-end law of the relay inbox: a message to an offline peer is held in the peer's inbox
//! carrier by whichever node owns it, handed to the peer with its storage interval when the peer
//! returns, delivered to the peer's application on stabilization, and compacted afterwards.

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

#[tokio::test]
async fn test_message_to_offline_peer_is_held_and_delivered_on_return() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node3 = prepare_node(key3).await;
    let offline: Did = key2.address().into();
    let inbox = inbox_key(offline);
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node3]).await;
    wait_for_successor(&node1, node3.did()).await?;

    // node1 routes the message to succ(offline) = node3, which is responsible for the offline
    // position and cannot deliver, so it holds the message in the inbox carrier `offline + 1`.
    // That key lies in node1's storage interval `(node1, node3]`, so the write lands at node1.
    node1
        .swarm
        .transport
        .send_message(Message::custom(HELD_MESSAGE)?, offline)
        .await?;
    let held = wait_for_storage_entry(&node1, inbox).await?;
    assert_eq!(held.kind, EntryKind::RelayMessage);
    assert_eq!(held.data.len(), 1);
    assert!(held
        .deliverable_inbox_messages(node1.swarm.network_id())
        .iter()
        .all(is_held_message));

    // The peer returns by joining through its successor node3. node2 notifies node3, node1's
    // next stabilization learns from node3 that node2 now precedes it, adopts node2 as successor,
    // and hands over the inbox carrier, whose key lies in node2's interval `(node2, node3]`.
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
    wait_for_storage_entry(&node2, inbox).await?;

    // Draining happens on the peer's own stabilization round.
    node2.swarm.stabilizer()?.stabilize().await?;

    let delivered = next_held_message(&node2).await?;
    assert_eq!(delivered.transaction.destination, node2.did());
    assert_eq!(delivered.transaction.signer(), node1.did());

    let compacted = wait_for_inbox_compaction(&node2, inbox).await?;
    assert!(compacted.crdt.register.is_some());
    Ok(())
}
