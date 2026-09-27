#[cfg(all(feature = "std", not(feature = "dummy"), not(target_family = "wasm")))]
use tokio::sync::watch;

use super::*;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::dht::topology;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::dht::StorageSyncDestination;
#[cfg(not(target_family = "wasm"))]
use crate::lifecycle::StopSource;
#[cfg(all(feature = "std", not(feature = "dummy"), not(target_family = "wasm")))]
use crate::storage::KvStorageInterface;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::activity::probe_on_activity;
use crate::tests::live_entry;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::midpoint_storage_key;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::ring_topology_converged;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::tail_storage_key;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn ensure_storage_repair_route(node: &Node, placement: Did, next_hop: Did) -> Result<()> {
    let destination = StorageSyncDestination::placement_key(placement);
    let observed = node.dht().next_hop_for_storage_sync(destination)?;
    if observed == Some(next_hop) {
        return Ok(());
    }

    Err(Error::InvalidMessage(format!(
        "storage repair fixture expected placement {placement} to route through {next_hop}, got {observed:?}",
    )))
}

#[tokio::test]
async fn test_stabilize_republishes_local_entries_to_missing_affine_owners() -> Result<()> {
    let key = SecretKey::random();
    let session = DelegateeKey::new_with_seckey(&key)?;
    let node = Node::build(
        SwarmBuilder::new(
            0,
            crate::tests::default::TEST_ICE_SERVERS,
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0),
    );
    let entry = live_entry(key.address().into(), vec![], EntryKind::Data);
    let placement_keys = entry.did.rotate_affine(2)?;
    node.dht()
        .storage
        .put(&placement_keys[0].to_string(), &entry)
        .await?;

    node.swarm.stabilizer().stabilize().await?;

    assert_eq!(
        node.dht()
            .storage
            .get(&placement_keys[1].to_string())
            .await?,
        Some(entry)
    );
    Ok(())
}

/// Hang guard of the continuous-repair convergence probe: the maintenance loop it waits on is
/// paced by its own 500 ms interval, so a converging run needs several rounds.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
const CONTINUOUS_REPAIR_HANG_GUARD: Duration = Duration::from_secs(15);

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_continuous_storage_repair_reaches_remote_owners_across_three_nodes() -> Result<()> {
    let node1 = prepare_repair_node(SecretKey::random())?;
    let node2 = prepare_repair_node(SecretKey::random())?;
    let node3 = prepare_repair_node(SecretKey::random())?;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    manually_establish_connection(&node2.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;

    let mut routed_peers = [&node2, &node3];
    routed_peers.sort_by_key(|node| topology::dist(node1.did(), node.did()));
    let [head, tail] = routed_peers;
    replace_observed_topology(&node1, &[head.did(), tail.did()], None, &[
        (0, head.did()),
        (3, tail.did()),
    ])?;

    let head_key = midpoint_storage_key(node1.did(), head.did(), tail.did());
    let tail_key = tail_storage_key(node1.did(), tail.did());
    ensure_storage_repair_route(&node1, head_key, head.did())?;
    ensure_storage_repair_route(&node1, tail_key, tail.did())?;

    let head_entry = live_entry(head_key, vec![], EntryKind::Data);
    let tail_entry = live_entry(tail_key, vec![], EntryKind::Data);
    let expected_head_entry = head_entry.clone().try_into_storage_entry()?;
    let expected_tail_entry = tail_entry.clone().try_into_storage_entry()?;
    node1
        .dht()
        .storage
        .put(&head_key.to_string(), &head_entry)
        .await?;
    node1
        .dht()
        .storage
        .put(&tail_key.to_string(), &tail_entry)
        .await?;

    assert_eq!(head.dht().storage.get(&head_key.to_string()).await?, None);
    assert_eq!(tail.dht().storage.get(&tail_key.to_string()).await?, None);

    let stop = StopSource::new();
    let maintenance_interval = Duration::from_millis(500);
    let maintenance = [
        node1.swarm.stabilizer(),
        node2.swarm.stabilizer(),
        node3.swarm.stabilizer(),
    ]
    .map(|stabilizer| {
        let token = stop.token();
        tokio::spawn(Arc::new(stabilizer).wait_with(maintenance_interval, token))
    });
    let pressure_swarm = node1.swarm.clone();
    let pressure_token = stop.token();
    let repair_pressure = tokio::spawn(async move {
        while !pressure_token.should_stop() {
            pressure_swarm.transport.request_storage_repair();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });

    let convergence = probe_on_activity(
        "maintenance loop converged under repair pressure",
        CONTINUOUS_REPAIR_HANG_GUARD,
        || async {
            let head_repaired = head.dht().storage.get(&head_key.to_string()).await?
                == Some(expected_head_entry.clone());
            let tail_repaired = tail.dht().storage.get(&tail_key.to_string()).await?
                == Some(expected_tail_entry.clone());
            let topology_converged = ring_topology_converged(&[
                node1.swarm.as_ref(),
                node2.swarm.as_ref(),
                node3.swarm.as_ref(),
            ])?;
            Ok(
                (head_repaired && tail_repaired && topology_converged).then_some((
                    head_repaired,
                    tail_repaired,
                    topology_converged,
                )),
            )
        },
    )
    .await;

    stop.request_stop();
    for task in maintenance {
        timeout(Duration::from_secs(3), task)
            .await
            .map_err(|_| Error::InvalidMessage("maintenance task did not stop".to_string()))?
            .map_err(|error| Error::InvalidMessage(format!("maintenance task failed: {error}")))?;
    }
    timeout(Duration::from_secs(3), repair_pressure)
        .await
        .map_err(|_| Error::InvalidMessage("repair pressure task did not stop".to_string()))?
        .map_err(|error| Error::InvalidMessage(format!("repair pressure task failed: {error}")))?;
    let (head_repaired, tail_repaired, topology_converged) = convergence?;

    assert!(
        head_repaired,
        "continuous repair did not persist the head owner placement"
    );
    assert!(
        tail_repaired,
        "continuous repair did not persist the tail owner placement"
    );
    assert!(
        topology_converged,
        "DHT control traffic did not converge the three-node ring during continuous repair"
    );
    Ok(())
}

/// Entry storage that publishes a write generation after every successful `put`.
///
/// The generation is a watch cell, so it is a stored state: a reader that marks the current
/// generation seen and then reads the store cannot miss a write that lands after its read.
#[cfg(all(feature = "std", not(feature = "dummy"), not(target_family = "wasm")))]
struct WriteSignalingStorage {
    inner: MemStorage<Entry>,
    writes: watch::Sender<u64>,
}

#[cfg(all(feature = "std", not(feature = "dummy"), not(target_family = "wasm")))]
#[async_trait]
impl KvStorageInterface<Entry> for WriteSignalingStorage {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &Entry) -> Result<()> {
        self.inner.put(key, value).await?;
        self.writes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, Entry)>> {
        self.inner.get_all().await
    }

    async fn remove(&self, key: &str) -> Result<()> {
        self.inner.remove(key).await
    }

    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }

    async fn count(&self) -> Result<u32> {
        self.inner.count().await
    }
}

/// Await the event "`node` stores `expected` at `key`", woken by `node`'s storage writes.
///
/// ```text
/// loop:  mark(generation)  ;  read(key) = expected ? return : await generation' ≠ generation
/// ```
///
/// Law (no lost wake-up): the generation is marked seen before the store is read, so a write
/// that lands after the read advances the generation past the mark and wakes `changed`.
#[cfg(all(feature = "std", not(feature = "dummy"), not(target_family = "wasm")))]
async fn await_stored_entry(
    node: &Node,
    writes: &mut watch::Receiver<u64>,
    key: Did,
    expected: &Entry,
) -> Result<()> {
    loop {
        writes.borrow_and_update();
        if node.dht().storage.get(&key.to_string()).await?.as_ref() == Some(expected) {
            return Ok(());
        }
        writes
            .changed()
            .await
            .map_err(|_| Error::InvalidMessage("storage write signal closed".to_string()))?;
    }
}

/// Native `wait_with` maintenance repairs a remote placement, and retiring the connection
/// while maintenance and repair pressure keep running releases every outbound transfer to the
/// retired peer and closes the physical connection.
///
/// ```text
/// Stored   ≡ node2.store[placement] = entry
/// Retired  ≡ K₀ deallocated, K₀ = node1's capacity toward node2 at the wait
///          ⟹ every transfer admitted to node2 through K₀ has released its permit
/// Closed   ≡ RTCPeerConnection::close() succeeded
///
/// (1)  maintenance ∥ repair_pressure             ⊢ ◇Stored      node2's write signal
/// (2)  Stored ; disconnect(node2) ∥ producers    ⊢ ◇Retired     capacity retirement witness
/// (3)  Stored ; disconnect(node2)                ⊢ ◇Closed      close witness
/// ```
///
/// All three predicates are stable. Nothing removes the placement. A close that succeeded
/// stays successful. Deallocation is terminal: once `K₀` is gone, a later reservation toward
/// node2 creates a new capacity and cannot revive `K₀`.
///
/// Liveness of (2) needs one instant with no permit of `K₀` held. A reservation that starts
/// while `K₀` is live joins it (`OutboundRegistry::capacity` upgrades the live `Weak`), so
/// continuously overlapping reservations would keep `K₀` alive. After `disconnect`, node2 is
/// in none of node1's successors, predecessor or fingers, so maintenance and repair choose no
/// next hop toward it. node2 runs no maintenance of its own, and its retired connection
/// admits no further inbound work, so nothing on node1 reserves toward it. A regression that
/// breaks either premise and keeps reserving toward the retired peer is exactly what the hang
/// guard reports.
///
/// No wake-up can be lost: each wait registers its listener before it reads the state. The
/// write generation is marked before the store is read, the retirement watch is subscribed
/// while `K₀` is pinned, and the close witness is taken before `disconnect`. Each is a stored
/// state, not a pulse.
///
/// The producers keep running through (2), so the test covers retirement racing live
/// maintenance.
///
/// No observation waits for a duration. The producers under test are timer-paced by design
/// (`wait_with(500 ms)`, a 25 ms pressure loop), and the timeouts only guard against a hang.
/// The setup still goes through the shared `wait_for_successor` and `wait_for_msgs` helpers;
/// their wall-clock dependence is tracked in #882.
#[cfg(all(feature = "std", not(feature = "dummy"), not(target_family = "wasm")))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_native_wait_with_repairs_storage_before_connection_retirement() -> Result<()> {
    let (key1, key2) = repair_test_keys()?;
    let node1 = prepare_repair_node(key1)?;
    let (writes, mut node2_writes) = watch::channel(0_u64);
    let node2 = prepare_repair_node_with_storage(
        key2,
        Box::new(WriteSignalingStorage {
            inner: MemStorage::new(),
            writes,
        }),
        None,
    )?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    replace_observed_topology(&node1, &[node2.did()], Some(node2.did()), &[(
        0,
        node2.did(),
    )])?;
    replace_observed_topology(&node2, &[node1.did()], Some(node1.did()), &[(
        0,
        node1.did(),
    )])?;

    let (entry, remote_placement) = entry_for_remote_repair_placement(&node1, node2.did())?;
    let expected = entry.clone().try_into_storage_entry()?;
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;
    assert_eq!(
        node2
            .dht()
            .storage
            .get(&remote_placement.to_string())
            .await?,
        None
    );

    // (1) Repair runs under concurrent maintenance and repair pressure.
    let stop = StopSource::new();
    let maintenance = {
        let token = stop.token();
        tokio::spawn(
            Arc::new(node1.swarm.stabilizer()).wait_with(Duration::from_millis(500), token),
        )
    };
    let repair_pressure = {
        let swarm = node1.swarm.clone();
        let token = stop.token();
        tokio::spawn(async move {
            while !token.should_stop() {
                swarm.transport.request_storage_repair();
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    };
    let repair = timeout(
        Duration::from_secs(30),
        await_stored_entry(&node2, &mut node2_writes, remote_placement, &expected),
    )
    .await
    .map_err(|_| Error::InvalidMessage("native wait_with repair did not persist".to_string()));

    // (2), (3) Retire the connection while both producers still run.
    let retirement = async {
        repair??;
        let physical_close = node1
            .swarm
            .transport
            .get_connection(node2.did())
            .ok_or_else(|| Error::InvalidMessage("missing native connection witness".to_string()))?
            .physical_close_witness()?;
        node1.swarm.disconnect(node2.did()).await?;
        assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
        assert!(!node1.swarm.transport.has_active_connection(node2.did()));
        // The liveness premise of (2): node2 is in none of node1's topology slots, so no
        // maintenance or repair step picks it as a next hop.
        assert!(!node1.dht().successors().contains(&node2.did())?);
        assert_ne!(*node1.dht().lock_predecessor()?, Some(node2.did()));
        assert!(!node1.dht().lock_finger()?.contains(Some(node2.did())));
        timeout(
            Duration::from_secs(3),
            node1
                .swarm
                .transport
                .outbound_capacity_retired_for_test(node2.did()),
        )
        .await
        .map_err(|_| {
            Error::InvalidMessage("retired peer's outbound capacity was not released".to_string())
        })?;
        let closed = timeout(Duration::from_secs(3), physical_close.completed())
            .await
            .map_err(|_| {
                Error::InvalidMessage(
                    "native physical connection close did not complete".to_string(),
                )
            })?;
        assert!(closed, "native physical connection close failed");
        Ok::<_, Error>(())
    }
    .await;

    stop.request_stop();
    timeout(Duration::from_secs(3), maintenance)
        .await
        .map_err(|_| Error::InvalidMessage("native maintenance task did not stop".to_string()))?
        .map_err(|error| Error::InvalidMessage(format!("maintenance task failed: {error}")))?;
    timeout(Duration::from_secs(3), repair_pressure)
        .await
        .map_err(|_| Error::InvalidMessage("native repair pressure did not stop".to_string()))?
        .map_err(|error| Error::InvalidMessage(format!("repair pressure failed: {error}")))?;
    retirement?;
    Ok(())
}

/// Law: a newly admitted, ready next hop can persist a replica immediately;
/// elapsed connection age is not an additional storage-repair prerequisite.
#[tokio::test]
async fn test_repair_storage_persists_replica_through_fresh_next_hop() -> Result<()> {
    // Ordered identities give the source a placement owned by the second node.
    let (key1, key2) = repair_test_keys()?;
    // The source starts with the entry; the receiver must acquire it through repair.
    let node1 = prepare_repair_node(key1)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    // The fixture must exercise the removed 30-second deferral window.
    let connected_for_ms = node1
        .swarm
        .transport
        .peer_connected_for_ms(node2.did(), get_epoch_ms_i64())?
        .ok_or_else(|| Error::InvalidMessage("missing peer admission age".to_string()))?;
    assert!(
        connected_for_ms < 30_000,
        "test requires a fresh connection, observed {connected_for_ms}ms"
    );

    // The remote affine placement is absent before the bounded repair pass.
    let (entry, remote_placement) = entry_for_remote_repair_placement(&node1, node2.did())?;
    // Compare the receiver's persisted canonical CRDT representation.
    let expected = entry.clone().try_into_storage_entry()?;
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;
    assert_eq!(
        node2
            .dht()
            .storage
            .get(&remote_placement.to_string())
            .await?,
        None
    );
    assert_eq!(
        node1.swarm.stabilizer().repair_storage().await?,
        crate::dht::StorageRepairOutcome::Complete
    );

    // Delivery completion alone does not prove that the receiving handler stored it.
    let slot = crate::dht::StorageKey::new(EntryKind::Data, remote_placement);
    assert_eq!(
        crate::tests::default::wait_for_storage_entry(&node2, slot).await?,
        expected
    );
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_repair_storage_defers_disconnected_open_transport_without_sending() -> Result<()> {
    let (key1, key2) = repair_test_keys()?;
    let node1 = prepare_repair_node(key1)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    let (entry, remote_placement) = entry_for_remote_repair_placement(&node1, node2.did())?;
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;
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

    let _pending_wait = PendingDataChannelWaitGuard::new();
    let outcome = timeout(
        Duration::from_millis(200),
        node1.swarm.stabilizer().repair_storage(),
    )
    .await
    .map_err(|_| Error::PromiseStateTimeout)??;
    assert_eq!(outcome, StorageRepairOutcome::Deferred);

    assert_no_more_msg([&node2]).await;
    assert_eq!(
        node2
            .dht()
            .storage
            .get(&remote_placement.to_string())
            .await?,
        None
    );
    assert!(node1.swarm.transport.has_active_connection(node2.did()));
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_repair_storage_backpressure_defers_without_degrading_or_removing_peer() -> Result<()>
{
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let (key1, key2) = repair_test_keys()?;
    let node1 = prepare_repair_node_with_measure(key1, measure_impl)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;

    node1.dht().successors().extend(&[node2.did()])?;
    *node1.dht().lock_predecessor()? = Some(node2.did());
    {
        let dht = node1.dht();
        let mut finger = dht.lock_finger()?;
        finger.set(0, node2.did());
        finger.set(3, node2.did());
    }

    let (entry, remote_placement) = entry_for_remote_repair_placement(&node1, node2.did())?;
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;

    let _pending_send = PendingSendGuard::new();
    assert_eq!(
        node1.swarm.stabilizer().repair_storage().await?,
        StorageRepairOutcome::Deferred
    );

    assert_no_more_msg([&node2]).await;
    assert_eq!(
        node2
            .dht()
            .storage
            .get(&remote_placement.to_string())
            .await?,
        None
    );
    assert_eq!(
        measure
            .event_count(node2.did(), MeasurementEvent::FailedToSend)
            .await,
        0
    );
    node1.swarm.transport.request_storage_repair();
    assert_eq!(
        node1
            .swarm
            .stabilizer()
            .run_requested_storage_maintenance()
            .await,
        Some(StorageRepairOutcome::Deferred)
    );
    assert!(
        node1.swarm.transport.storage_repair_requested(),
        "a deferred maintenance delivery must preserve repair intent"
    );

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.get_connection(node2.did()).is_some());
    assert!(node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
    assert!(node1.dht().lock_finger()?.contains(Some(node2.did())));
    assert_eq!(
        measure
            .event_count(node2.did(), MeasurementEvent::FailedToSend)
            .await,
        0
    );
    Ok(())
}
