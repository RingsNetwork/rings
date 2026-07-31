use super::*;

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

#[tokio::test]
async fn repair_storage_defers_sync_to_fresh_next_hop() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
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

    let (entry, remote_placement) = entry_with_remote_repair_placement(&node1)?;
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
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_repair_node(key1)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    node1
        .swarm
        .transport
        .force_peer_connected_at(node2.did(), get_epoch_ms_i64() - 31_000)?;

    let (entry, remote_placement) = entry_with_remote_repair_placement(&node1)?;
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
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
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

    let (entry, remote_placement) = entry_with_remote_repair_placement(&node1)?;
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
