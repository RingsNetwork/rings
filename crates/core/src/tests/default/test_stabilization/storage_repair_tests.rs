#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use num_bigint::BigUint;

use super::*;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::dht::topology;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::dht::StorageSyncDestination;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn midpoint_storage_key(local: Did, lower: Did, upper: Did) -> Did {
    let midpoint =
        (topology::dist(local, lower) + topology::dist(local, upper)) / BigUint::from(2_u8);
    local + Did::from(midpoint)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn tail_storage_key(local: Did, lower: Did) -> Did {
    let ring_size = BigUint::from(1_u8) << 160usize;
    let midpoint = (topology::dist(local, lower) + ring_size) / BigUint::from(2_u8);
    local + Did::from(midpoint)
}

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
async fn stabilize_republishes_local_entries_to_missing_affine_owners() -> Result<()> {
    let key = SecretKey::random();
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0)
        .build(),
    );
    let node = Node::new(swarm);
    let entry = Entry::new(key.address().into(), vec![], EntryKind::Data);
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

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn continuous_storage_repair_reaches_remote_owners_across_three_nodes() -> Result<()> {
    let node1 = prepare_repair_node(SecretKey::random())?;
    let node2 = prepare_repair_node(SecretKey::random())?;
    let node3 = prepare_repair_node(SecretKey::random())?;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;

    let mut routed_peers = [&node2, &node3];
    routed_peers.sort_by_key(|node| topology::dist(node1.did(), node.did()));
    let [head, tail] = routed_peers;
    replace_observed_topology(&node1, &[head.did(), tail.did()], None, &[
        (0, head.did()),
        (3, tail.did()),
    ])?;
    for peer in [head.did(), tail.did()] {
        node1
            .swarm
            .transport
            .force_peer_connected_at(peer, get_epoch_ms_i64() - 31_000)?;
    }

    let head_key = midpoint_storage_key(node1.did(), head.did(), tail.did());
    let tail_key = tail_storage_key(node1.did(), tail.did());
    ensure_storage_repair_route(&node1, head_key, head.did())?;
    ensure_storage_repair_route(&node1, tail_key, tail.did())?;

    let head_entry = Entry::new(head_key, vec![], EntryKind::Data);
    let tail_entry = Entry::new(tail_key, vec![], EntryKind::Data);
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

    let mut head_repaired = false;
    let mut tail_repaired = false;
    for _ in 0..12 {
        let _ = node1.swarm.stabilizer().repair_storage().await?;
        wait_for_msgs([&node1, &node2, &node3]).await;

        head_repaired = head.dht().storage.get(&head_key.to_string()).await?
            == Some(expected_head_entry.clone());
        tail_repaired = tail.dht().storage.get(&tail_key.to_string()).await?
            == Some(expected_tail_entry.clone());
        if head_repaired && tail_repaired {
            break;
        }
    }

    assert!(
        head_repaired,
        "continuous repair did not persist the head owner placement"
    );
    assert!(
        tail_repaired,
        "continuous repair did not persist the tail owner placement"
    );
    Ok(())
}

#[tokio::test]
async fn repair_storage_defers_sync_to_fresh_next_hop() -> Result<()> {
    let (key1, key2) = repair_test_keys()?;
    let node1 = prepare_repair_node(key1)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    let connected_for_ms = node1
        .swarm
        .transport
        .peer_connected_for_ms(node2.did(), get_epoch_ms_i64())?
        .ok_or_else(|| Error::InvalidMessage("missing peer admission age".to_string()))?;
    assert!(
        connected_for_ms < 30_000,
        "test must exercise a fresh connection; observed age {connected_for_ms}ms"
    );

    let (entry, remote_placement) = entry_for_remote_repair_placement(&node1, node2.did())?;
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;

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
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn repair_storage_defers_disconnected_open_transport_without_sending() -> Result<()> {
    let (key1, key2) = repair_test_keys()?;
    let node1 = prepare_repair_node(key1)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    node1
        .swarm
        .transport
        .force_peer_connected_at(node2.did(), get_epoch_ms_i64() - 31_000)?;

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
    assert!(node1.swarm.transport.is_admitted_connection(node2.did()));
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn repair_storage_backpressure_defers_without_degrading_or_removing_peer() -> Result<()> {
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let (key1, key2) = repair_test_keys()?;
    let node1 = prepare_repair_node_with_measure(key1, measure_impl)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    node1
        .swarm
        .transport
        .force_peer_connected_at(node2.did(), get_epoch_ms_i64() - 31_000)?;

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
            .get_count(node2.did(), MeasureCounter::FailedToSend)
            .await,
        0
    );
    node1.swarm.transport.request_storage_repair();
    assert_eq!(
        node1
            .swarm
            .stabilizer()
            .run_requested_storage_repair()
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
            .get_count(node2.did(), MeasureCounter::FailedToSend)
            .await,
        0
    );
    Ok(())
}
