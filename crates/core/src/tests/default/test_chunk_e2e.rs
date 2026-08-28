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
use crate::dht::successor::SuccessorReader;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::ApplyOutcome;
use crate::measure::Authentication;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::measure::MeasureError;
use crate::measure::MeasureImpl;
use crate::measure::MeasurementBatch;
use crate::measure::MeasurementEvent;
use crate::measure::PeerQuality;
use crate::message::Message;
use crate::message::MessageClass;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::PeerLivenessProbe;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::TrackedStorageSyncOutcome;
use crate::tests::assert_control_interleaves_transfer;
use crate::tests::default::dummy_hooks::MaxMessageSizeGuard;
use crate::tests::default::dummy_hooks::PausedDeliveryGuard;
use crate::tests::default::dummy_hooks::PausedDispatchGuard;
use crate::tests::default::dummy_hooks::PendingAfterSentCountGuard;
use crate::tests::default::dummy_hooks::PendingCloseGuard;
use crate::tests::default::dummy_hooks::PendingDeliveryGuard;
use crate::tests::default::dummy_hooks::PendingSendGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::prepare_node_with_measure;
use crate::tests::default::wait_for_connection_state;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::manually_establish_connection;
use crate::tests::multi_frame_storage_sync_entries;
use crate::tests::outbound_capacity_released;

#[derive(Default)]
struct CountingMeasure {
    counters: Mutex<Vec<(crate::dht::Did, MeasureCounter)>>,
    events: Mutex<Vec<(crate::dht::Did, MeasurementEvent)>>,
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

    fn logical_transfer_count(
        &self,
        did: crate::dht::Did,
        received: bool,
        expected_useful_bytes: u64,
    ) -> usize {
        match self.events.lock() {
            Ok(events) => events
                .iter()
                .filter(|(observed_did, event)| match event {
                    MeasurementEvent::Sent { useful_bytes } => {
                        *observed_did == did && !received && *useful_bytes == expected_useful_bytes
                    }
                    MeasurementEvent::Received { useful_bytes } => {
                        *observed_did == did && received && *useful_bytes == expected_useful_bytes
                    }
                    _ => false,
                })
                .count(),
            Err(_) => 0,
        }
    }
}

#[async_trait]
impl Measure for CountingMeasure {
    async fn incr(
        &self,
        did: crate::dht::Did,
        authentication: Authentication,
        counter: MeasureCounter,
    ) {
        if matches!(authentication, Authentication::Unauthenticated) {
            return;
        }
        if let Ok(mut counters) = self.counters.lock() {
            counters.push((did, counter));
        }
    }

    async fn get_count(&self, did: crate::dht::Did, counter: MeasureCounter) -> u64 {
        self.count(did, counter)
    }

    async fn record(
        &self,
        did: crate::dht::Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        if matches!(authentication, Authentication::Unauthenticated) {
            return Ok(ApplyOutcome::IgnoredUnauthenticated);
        }
        if let Ok(mut events) = self.events.lock() {
            events.push((did, event));
        }
        self.incr(did, authentication, MeasureCounter::from_event(event))
            .await;
        Ok(ApplyOutcome::Applied)
    }

    async fn record_batch(
        &self,
        did: crate::dht::Did,
        authentication: Authentication,
        batch: MeasurementBatch,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        if matches!(authentication, Authentication::Unauthenticated) {
            return Ok(ApplyOutcome::IgnoredUnauthenticated);
        }
        for _ in 0..batch.occurrences().get() {
            self.record(did, authentication, batch.event()).await?;
        }
        Ok(ApplyOutcome::Applied)
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

async fn wait_for_test_condition(description: &'static str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"));
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
async fn test_whole_and_chunked_payloads_have_identical_measurement_delta() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let sender_measure = Arc::new(CountingMeasure::default());
    let receiver_measure = Arc::new(CountingMeasure::default());
    let sender_impl: MeasureImpl = sender_measure.clone();
    let receiver_impl: MeasureImpl = receiver_measure.clone();
    let node1 = prepare_node_with_measure(key1, sender_impl).unwrap();
    let node2 = prepare_node_with_measure(key2, receiver_impl).unwrap();
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();
    wait_for_connection_state(&node2, node1.did(), WebrtcConnectionState::Connected)
        .await
        .unwrap();

    // This is whole under the default negotiated limit and chunked under 8192 bytes.
    let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    let expected_useful_bytes = u64::try_from(
        rings_codec::serialize(&Message::custom(&big).unwrap())
            .unwrap()
            .len(),
    )
    .unwrap();
    let sent_before =
        sender_measure.logical_transfer_count(node2.did(), false, expected_useful_bytes);
    let received_before =
        receiver_measure.logical_transfer_count(node1.did(), true, expected_useful_bytes);

    node1
        .swarm
        .send_direct_message(Message::custom(&big).unwrap(), node2.did())
        .await
        .expect("whole send should succeed");
    let whole = recv_custom(&node2).await.expect("whole custom message");
    assert_eq!(whole, big);
    wait_for_test_condition("whole send must be measured once", || {
        sender_measure.logical_transfer_count(node2.did(), false, expected_useful_bytes)
            > sent_before
    })
    .await;
    assert_eq!(
        sender_measure.logical_transfer_count(node2.did(), false, expected_useful_bytes),
        sent_before + 1
    );
    assert_eq!(
        receiver_measure.logical_transfer_count(node1.did(), true, expected_useful_bytes),
        received_before + 1
    );

    let _max_size = MaxMessageSizeGuard::new(8192);
    node1
        .swarm
        .send_direct_message(Message::custom(&big).unwrap(), node2.did())
        .await
        .expect("chunked send should succeed");
    let chunked = recv_custom(&node2)
        .await
        .expect("reassembled custom message");
    assert_eq!(chunked, big);
    wait_for_test_condition("chunked send must be measured once", || {
        sender_measure.logical_transfer_count(node2.did(), false, expected_useful_bytes)
            >= sent_before + 2
    })
    .await;
    assert_eq!(
        sender_measure.logical_transfer_count(node2.did(), false, expected_useful_bytes),
        sent_before + 2,
        "whole and chunked sends must each emit one exact logical measurement"
    );

    assert_eq!(
        receiver_measure.logical_transfer_count(node1.did(), true, expected_useful_bytes),
        received_before + 2,
        "whole and reassembled payloads must expose the same useful-byte delta"
    );
}

#[tokio::test]
async fn test_spawned_storage_sync_tail_cancelled_by_route_disappear_does_not_degrade_next_hop(
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
async fn test_spawned_storage_sync_tail_cancels_when_transport_loses_readiness() -> Result<()> {
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
        "the first chunk must be accepted before readiness is withdrawn"
    );

    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            node2.did(),
            WebrtcConnectionState::Disconnected,
        )?;
    node1
        .swarm
        .transport
        .force_peer_data_channel_open_without_callback(node2.did(), Some(true))?;
    sleep(Duration::from_millis(100)).await;

    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "a recovering transport must cancel the chunk tail before another admission"
    );
    assert!(node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(node1.dht().successors().contains(&node2.did())?);
    assert_eq!(
        measure.count(node2.did(), MeasureCounter::FailedToSend),
        failed_before,
        "transient readiness loss must not itself degrade the peer"
    );

    node1.swarm.transport.disconnect(node2.did()).await?;
    Ok(())
}

#[tokio::test]
async fn test_spawned_chunk_tail_cancels_when_same_peer_is_readmitted() -> Result<()> {
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
    assert_eq!(dummy_controlled::sent_count(), 1);

    let (old, replacement) = node1
        .swarm
        .transport
        .replace_active_generation_for_test(node2.did())?;
    assert_ne!(old, replacement);
    sleep(Duration::from_millis(100)).await;

    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "an old-generation send must not dispatch through the replacement admission"
    );
    assert!(
        node1
            .swarm
            .transport
            .is_admitted_connection_attempt(replacement),
        "the replacement generation must remain admitted"
    );
    assert_eq!(
        measure.count(node2.did(), MeasureCounter::FailedToSend),
        failed_before,
        "revoking an old send capability is not a failure of the replacement peer"
    );

    node1.swarm.transport.disconnect(node2.did()).await?;
    Ok(())
}

#[tokio::test]
async fn test_send_waiting_at_dispatch_rechecks_the_admitted_generation() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_msgs([&node1, &node2]).await;

    dummy_controlled::reset_sent_count();
    let dispatch = PausedDispatchGuard::new();
    let swarm = Arc::clone(&node1.swarm);
    let peer = node2.did();
    let send = tokio::spawn(async move {
        swarm
            .send_message(Message::custom(b"stale-generation")?, peer)
            .await
    });
    wait_for_test_condition(
        "the send must reach the final pre-dispatch boundary",
        dummy_controlled::send_message_waiting_at_dispatch,
    )
    .await;

    let (old, replacement) = node1
        .swarm
        .transport
        .replace_active_generation_for_test(peer)?;
    assert_ne!(old, replacement);
    drop(dispatch);

    let result = tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("releasing the dispatch gate must wake the send")
        .expect("the send task must not panic");
    assert!(
        matches!(
            result,
            Err(crate::error::Error::ConnectionAttemptSuperseded {
                peer: superseded_peer,
                generation,
            }) if superseded_peer == peer && generation == old.generation()
        ),
        "the old send must report generation revocation, got {result:?}"
    );
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "a revoked generation must not cross the irreversible dispatch boundary"
    );

    node1.swarm.transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn test_storage_sync_waiting_at_dispatch_defers_when_its_route_disappears() -> Result<()> {
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    dummy_controlled::reset_sent_count();
    let dispatch = PausedDispatchGuard::new();
    let transport = Arc::clone(&node1.swarm.transport);
    let peer = node2.did();
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(peer),
        data: Vec::new(),
    };
    let send = tokio::spawn(async move { transport.send_storage_sync_tracked(msg).await });
    wait_for_test_condition(
        "the storage sync must reach the final pre-dispatch boundary",
        dummy_controlled::send_message_waiting_at_dispatch,
    )
    .await;

    let failed_before = measure.count(peer, MeasureCounter::FailedToSend);
    node1.dht().remove(peer)?;
    drop(dispatch);

    let outcome = tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("route cancellation must wake the gated send")
        .expect("the storage sync task must not panic")?;
    assert_eq!(outcome, TrackedStorageSyncOutcome::Deferred);
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "a revoked storage route must stop before dispatch"
    );
    assert_eq!(
        measure.count(peer, MeasureCounter::FailedToSend),
        failed_before,
        "route revocation is not evidence of peer failure"
    );

    node1.swarm.transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn test_storage_sync_waiting_at_dispatch_defers_when_transport_loses_readiness() -> Result<()>
{
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    dummy_controlled::reset_sent_count();
    let dispatch = PausedDispatchGuard::new();
    let transport = Arc::clone(&node1.swarm.transport);
    let peer = node2.did();
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(peer),
        data: Vec::new(),
    };
    let send = tokio::spawn(async move { transport.send_storage_sync_tracked(msg).await });
    wait_for_test_condition(
        "the storage sync must reach the final pre-dispatch boundary",
        dummy_controlled::send_message_waiting_at_dispatch,
    )
    .await;

    let failed_before = measure.count(peer, MeasureCounter::FailedToSend);
    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Disconnected)?;
    drop(dispatch);

    let outcome = tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("readiness cancellation must wake the gated send")
        .expect("the storage sync task must not panic")?;
    assert_eq!(outcome, TrackedStorageSyncOutcome::Deferred);
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "a non-ready transport must stop before dispatch"
    );
    assert_eq!(
        measure.count(peer, MeasureCounter::FailedToSend),
        failed_before,
        "transient transport readiness loss is not peer failure evidence"
    );

    node1.swarm.transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn test_detached_storage_sync_missing_routable_transport_does_not_degrade_peer() -> Result<()>
{
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    let peer = node2.did();
    let failed_before = measure.count(peer, MeasureCounter::FailedToSend);
    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Closed)?;
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(peer),
        data: Vec::new(),
    };

    assert!(node1
        .swarm
        .transport
        .send_storage_sync(msg)
        .await?
        .is_deferred());
    assert_eq!(
        measure.count(peer, MeasureCounter::FailedToSend),
        failed_before,
        "a missing storage data-plane route is a deferral, not peer failure evidence"
    );

    node1.swarm.transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn test_tracked_storage_sync_does_not_finish_while_a_chunk_tail_is_pending() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
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
    let transport = node1.swarm.transport.clone();
    let send = tokio::spawn(async move { transport.send_storage_sync_tracked(msg).await });

    wait_for_test_condition("the tracked path must enter the real chunk tail", || {
        dummy_controlled::sent_count() == 1
    })
    .await;
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "the tracked path must enter the real chunk tail"
    );
    assert!(
        !send.is_finished(),
        "tracked storage sync must not report completion after first-chunk admission"
    );

    node1.dht().remove(node2.did())?;
    let send_result = tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("route cancellation should release the tracked chunk tail")
        .expect("tracked storage sync task should not panic");
    assert_eq!(send_result?, TrackedStorageSyncOutcome::Deferred);
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "route cancellation must prevent any additional chunk admission"
    );
    Ok(())
}

#[tokio::test]
async fn test_tracked_storage_sync_timeout_closes_stalled_delivery_generation() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    dummy_controlled::reset_sent_count();
    let pending_delivery = PendingDeliveryGuard::new();
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: Vec::new(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        node1.swarm.transport.send_storage_sync_tracked(msg),
    )
    .await
    .expect("tracked delivery deadline must bound a stuck delivery future")?;

    assert_eq!(outcome, TrackedStorageSyncOutcome::Deferred);
    assert_eq!(dummy_controlled::sent_count(), 1);
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(
        outbound_capacity_released(&node1.swarm.transport, node2.did()),
        "tracked cancellation must release capacity before returning"
    );

    drop(pending_delivery);
    assert_eq!(dummy_controlled::sent_count(), 1);
    Ok(())
}

#[tokio::test]
async fn test_tracked_cleanup_grace_terminalizes_a_nonresponsive_generation() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    let peer = node2.did();
    let attempt = node1
        .swarm
        .transport
        .active_attempt(peer)?
        .ok_or(Error::ConnectionNotFound)?;
    let connection = node1
        .swarm
        .transport
        .get_connection(peer)
        .ok_or(Error::ConnectionNotFound)?;
    let _pending_delivery = PendingDeliveryGuard::new();
    let _pending_close = PendingCloseGuard::new();
    let payload = MessagePayload::new_send(
        Message::custom(b"tracked-cleanup-grace")?,
        node1.swarm.transport.session_sk(),
        peer,
        peer,
    )?;

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        node1
            .swarm
            .transport
            .send_payload_tracked_with_matching_delivery_deadline_for_test(payload),
    )
    .await
    .expect("tracked cleanup grace must bound a nonresponsive generation")
    .expect_err("nonresponsive cleanup must return its typed timeout");

    assert!(matches!(
        error,
        Error::TrackedPayloadCleanupTimeout { peer: failed, .. } if failed == peer
    ));
    assert_eq!(node1.swarm.transport.active_attempt(peer)?, None);
    assert!(!node1.swarm.transport.is_send_terminal_attempt(attempt)?);
    assert!(node1.swarm.transport.get_connection(peer).is_none());
    assert_eq!(
        connection.webrtc_connection_state(),
        WebrtcConnectionState::Closed
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !outbound_capacity_released(&node1.swarm.transport, peer) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal cleanup must release retained transfer capacity");
    Ok(())
}

#[tokio::test]
async fn test_dropping_tracked_storage_sync_requests_transfer_stop() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    dummy_controlled::reset_sent_count();
    let pending_delivery = PendingDeliveryGuard::new();
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: Vec::new(),
    };
    let transport = node1.swarm.transport.clone();
    let send = tokio::spawn(async move { transport.send_storage_sync_tracked(msg).await });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if dummy_controlled::sent_count() == 1
                && node1
                    .swarm
                    .transport
                    .outbound_admitted_transfer_count_for_test(node2.did())
                    == Some(1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tracked send must reach the stalled delivery");

    send.abort();
    let _ = send.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if node1.swarm.transport.get_connection(node2.did()).is_none()
                && outbound_capacity_released(&node1.swarm.transport, node2.did())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping a tracked send must stop and release its transfer");

    drop(pending_delivery);
    assert_eq!(dummy_controlled::sent_count(), 1);
    Ok(())
}

#[tokio::test]
async fn test_dht_control_frame_runs_while_storage_transfer_waits_for_delivery() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    let _max_size = MaxMessageSizeGuard::new(8192);
    let _paused_delivery = PausedDeliveryGuard::new();
    dummy_controlled::reset_sent_count();
    node1
        .swarm
        .transport
        .start_outbound_frame_trace_for_test(node2.did());

    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: multi_frame_storage_sync_entries()?,
    };
    assert!(node1
        .swarm
        .transport
        .send_storage_sync(msg)
        .await?
        .is_sent());
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "detached storage send should admit exactly its first frame"
    );
    let first = node2
        .listen_once()
        .await
        .ok_or_else(|| crate::error::Error::InvalidMessage("expected storage chunk".to_string()))?;
    assert!(
        matches!(first.transaction.data::<Message>()?, Message::Chunk(_)),
        "the first delivered frame should be the storage chunk envelope"
    );

    node1
        .swarm
        .send_message(
            Message::PeerLivenessProbe(PeerLivenessProbe { sent_at_ms: 7 }),
            node2.did(),
        )
        .await?;
    assert_eq!(
        dummy_controlled::sent_count(),
        2,
        "DHT control should not wait behind the bulk transfer's pending tail"
    );
    let second = node2.listen_once().await.ok_or_else(|| {
        crate::error::Error::InvalidMessage("expected DHT control frame".to_string())
    })?;
    assert!(
        matches!(
            second.transaction.data::<Message>()?,
            Message::PeerLivenessProbe(_)
        ),
        "new control work must be admitted before any new bulk frame"
    );
    dummy_controlled::release_delivery_future_gate();
    wait_for_test_condition(
        "the storage tail must resume after control admission",
        || {
            node1
                .swarm
                .transport
                .outbound_frame_trace_for_test(node2.did())
                .iter()
                .filter(|(class, _, _)| *class == MessageClass::Storage)
                .count()
                >= 2
        },
    )
    .await;
    let trace = node1
        .swarm
        .transport
        .take_outbound_frame_trace_for_test(node2.did());
    assert_control_interleaves_transfer(&trace, MessageClass::Storage);
    Ok(())
}

#[tokio::test]
async fn test_send_queue_backpressure_returns_transport_timeout() {
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
async fn test_negotiated_size_too_small_errors_without_partial_send() {
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
