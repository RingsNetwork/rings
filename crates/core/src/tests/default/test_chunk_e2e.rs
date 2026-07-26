//! End-to-end chunking over the dummy backend: by overriding the negotiated `max_message_size`
//! (via the dummy test hook) we force `do_send_payload` down the real chunked path and verify the
//! receiver reassembles the original message — exercising stream → wrap → send → reassemble, not
//! just the pure `WireReserves::plan` decision.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::sleep;
use tokio::time::Duration;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::measure::PeerQuality;
use crate::message::Message;
use crate::message::SyncEntriesWithSuccessor;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::SwarmBuilder;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_connection_state;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

struct PendingSendGuard;

impl PendingSendGuard {
    fn new() -> Self {
        dummy_controlled::set_send_message_pending(true);
        Self
    }
}

impl Drop for PendingSendGuard {
    fn drop(&mut self) {
        dummy_controlled::set_send_message_pending(false);
    }
}

struct PendingAfterSentCountGuard;

impl PendingAfterSentCountGuard {
    fn new(threshold: usize) -> Self {
        dummy_controlled::set_send_message_pending_after_sent_count(Some(threshold));
        Self
    }
}

impl Drop for PendingAfterSentCountGuard {
    fn drop(&mut self) {
        dummy_controlled::set_send_message_pending_after_sent_count(None);
    }
}

struct MaxMessageSizeGuard;

impl MaxMessageSizeGuard {
    fn new(size: usize) -> Self {
        dummy_controlled::set_max_message_size(size);
        Self
    }
}

impl Drop for MaxMessageSizeGuard {
    fn drop(&mut self) {
        dummy_controlled::set_max_message_size(0);
    }
}

#[derive(Default)]
struct CountingMeasure {
    counters: Mutex<Vec<(crate::dht::Did, MeasureCounter)>>,
}

impl CountingMeasure {
    fn count(&self, did: crate::dht::Did, counter: MeasureCounter) -> u64 {
        match self.counters.lock() {
            Ok(counters) => counters
                .iter()
                .filter(|(observed_did, observed_counter)| {
                    *observed_did == did && *observed_counter == counter
                })
                .count() as u64,
            Err(_) => 0,
        }
    }
}

#[async_trait]
impl Measure for CountingMeasure {
    async fn incr(&self, did: crate::dht::Did, counter: MeasureCounter) {
        if let Ok(mut counters) = self.counters.lock() {
            counters.push((did, counter));
        }
    }

    async fn get_count(&self, did: crate::dht::Did, counter: MeasureCounter) -> u64 {
        self.count(did, counter)
    }
}

#[async_trait]
impl BehaviourJudgement for CountingMeasure {
    async fn quality(&self, _did: crate::dht::Did) -> PeerQuality {
        PeerQuality::Unknown
    }

    async fn good(&self, _did: crate::dht::Did) -> bool {
        true
    }
}

fn prepare_node_with_measure(key: SecretKey, measure: MeasureImpl) -> Result<Node> {
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_finger_table_size(super::TEST_DHT_FINGER_TABLE_SIZE)
        .dht_virtual_nodes(0)
        .measure(measure)
        .build(),
    );
    Ok(Node::new(swarm))
}

fn large_storage_sync_entries() -> Result<Vec<PlacedEntry>> {
    let mut entries = Vec::new();
    for index in 0..48 {
        let topic = format!("chunked storage sync cancellation {index}");
        let entry_did = Entry::gen_did(&topic)?;
        let payload = format!("payload-{index}-{}", "x".repeat(512));
        let entry = Entry::new(entry_did, vec![payload.into()], EntryKind::Data);
        entries.push(PlacedEntry::new(entry_did, entry));
    }
    Ok(entries)
}

/// Read inbound messages on `node` until a `CustomMessage` arrives (skipping DHT bookkeeping), or
/// give up after a bounded number of messages.
async fn recv_custom(node: &crate::tests::default::Node) -> Option<Vec<u8>> {
    for _ in 0..64 {
        let payload = node.listen_once().await?;
        if let Ok(Message::CustomMessage(cm)) = payload.transaction.data() {
            return Some(cm.0);
        }
    }
    None
}

#[tokio::test]
async fn large_message_is_chunked_and_reassembled() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();
    wait_for_connection_state(&node2, node1.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();

    // Force a small negotiated limit so the payload below must be chunked. Set it *after* the
    // handshake so the connect offer/answer themselves are unaffected.
    dummy_controlled::set_max_message_size(8192);

    // Comfortably larger than the negotiated limit → many chunks.
    let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    node1
        .swarm
        .send_message(Message::custom(&big).unwrap(), node2.did())
        .await
        .expect("send should succeed and chunk");

    let got = recv_custom(&node2)
        .await
        .expect("reassembled custom message");
    assert_eq!(
        got, big,
        "receiver must reassemble the exact original payload"
    );

    dummy_controlled::set_max_message_size(0);
}

#[tokio::test]
async fn spawned_storage_sync_tail_cancelled_by_route_disappear_does_not_degrade_next_hop(
) -> Result<()> {
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    let _max_size = MaxMessageSizeGuard::new(8192);
    dummy_controlled::reset_sent_count();
    let _pending_after_first_chunk = PendingAfterSentCountGuard::new(1);
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: large_storage_sync_entries()?,
    };

    let failed_before = measure.count(node2.did(), MeasureCounter::FailedToSend);
    node1.swarm.transport.send_storage_sync(msg).await?;
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "the first chunk is accepted before the route is withdrawn"
    );

    node1.dht().remove(node2.did())?;
    sleep(Duration::from_millis(600)).await;
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "route cancellation must stop the spawned task before another chunk is dispatched"
    );
    assert_eq!(
        measure.count(node2.did(), MeasureCounter::FailedToSend),
        failed_before,
        "route cancellation is stale topology, not next-hop transport failure"
    );
    Ok(())
}

#[tokio::test]
async fn send_queue_backpressure_returns_transport_timeout() {
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node_with_measure(key1, measure_impl).unwrap();
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();
    wait_for_connection_state(&node2, node1.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();

    let failed_before = measure.count(node2.did(), MeasureCounter::FailedToSend);
    let _guard = PendingSendGuard::new();
    let err = node1
        .swarm
        .send_message(Message::custom(b"queue-blocked").unwrap(), node2.did())
        .await
        .expect_err("send admission should time out when the backend never accepts bytes");

    assert!(
        matches!(
            err,
            crate::error::Error::DataChannelSendQueueTimeout { peer, .. } if peer == node2.did()
        ),
        "expected DataChannelSendQueueTimeout, got {err:?}"
    );
    assert_eq!(
        measure.count(node2.did(), MeasureCounter::FailedToSend),
        failed_before,
        "data-channel queue backpressure is local admission pressure, not peer failure"
    );
}

#[tokio::test]
async fn negotiated_size_too_small_errors_without_partial_send() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();
    wait_for_connection_state(&node2, node1.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();

    // Below `chunk_overhead + MIN_CHUNK_DATA`: no usable chunk size exists, so framing must reject
    // *before* any chunk is sent (the `None` is returned ahead of the send loop).
    dummy_controlled::set_max_message_size(5000);

    let big: Vec<u8> = vec![0xab; 10_000];
    // Count data-channel sends from here, to prove the failed send enqueues nothing.
    dummy_controlled::reset_sent_count();
    let err = node1
        .swarm
        .send_message(Message::custom(&big).unwrap(), node2.did())
        .await
        .expect_err("an unusably small negotiated size must fail the send");
    assert!(
        matches!(err, crate::error::Error::PeerMaxMessageSizeTooSmall(_)),
        "expected PeerMaxMessageSizeTooSmall, got {err:?}"
    );
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "no chunk (partial or otherwise) must be dispatched when framing rejects the size"
    );

    dummy_controlled::set_max_message_size(0);
}
