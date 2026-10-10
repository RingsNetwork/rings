use super::*;

#[tokio::test]
async fn test_periodic_republish_restores_missing_local_affine_replica() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let (first_key, second_key) = first_two_affine_keys(entry.did)?;
    node.storage.put(&first_key.to_string(), &entry).await?;

    let action = node.republish_local_entries(2).await?;

    assert_eq!(action, PeerRingAction::None);
    assert_eq!(
        node.storage.get(&first_key.to_string()).await?,
        Some(entry.clone())
    );
    assert_eq!(
        node.storage.get(&second_key.to_string()).await?,
        Some(entry)
    );
    Ok(())
}

#[tokio::test]
async fn test_republish_joins_local_branch_and_routes_remote_placement_keys() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let successor = Did::from(100u32);
    node.successors().update(successor)?;
    let entry = data_entry(Did::from(10u32));
    let (first_key, second_key) = first_two_affine_keys(entry.did)?;
    node.storage.put(&entry.did.to_string(), &entry).await?;

    let action = node.republish_local_entries(2).await?;

    assert_eq!(
        action,
        PeerRingAction::MultiActions(vec![PeerRingAction::RemoteAction(
            second_key,
            RemoteAction::SyncEntriesWithSuccessor {
                purpose: StorageSyncPurpose::AdditiveRepair,
                route: StorageSyncRoute::PlacementKey,
                data: vec![PlacedEntry::new(second_key, entry.clone())],
            }
        )])
    );
    assert_eq!(
        node.storage.get(&first_key.to_string()).await?,
        Some(entry.clone())
    );
    assert_eq!(node.storage.get(&second_key.to_string()).await?, None);
    Ok(())
}

#[tokio::test]
async fn test_read_repair_is_noop_for_single_replica_storage() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));

    let action = node.read_repair_entry(entry, &[], 1).await?;

    assert_eq!(action, PeerRingAction::None);
    assert_eq!(node.storage.count().await?, 0);
    Ok(())
}

#[tokio::test]
async fn test_local_hit_lookup_has_no_read_repair_targets() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let mut placement_keys = entry.did.rotate_affine(2)?.into_iter();
    let first_key = placement_keys
        .next()
        .ok_or_else(|| Error::InvalidMessage("expected first placement".to_string()))?;
    node.storage.put(&first_key.to_string(), &entry).await?;

    let action = node.entry_lookup(entry.did, 2).await?;
    let evidence = match action {
        PeerRingAction::SomeEntry(evidence) => evidence,
        action => return Err(Error::unexpected_peer_ring_action(action)),
    };
    let repair = node
        .read_repair_entry(evidence.entry.clone(), &evidence.misses, 2)
        .await?;

    assert!(evidence.misses.is_empty());
    assert_eq!(repair, PeerRingAction::None);
    assert_eq!(node.storage.count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn test_read_repair_targets_only_observed_missing_placements() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let placement_keys = entry.did.rotate_affine(3)?;
    let first_key = *placement_keys
        .first()
        .ok_or_else(|| Error::InvalidMessage("expected first placement".to_string()))?;
    let second_key = *placement_keys
        .get(1)
        .ok_or_else(|| Error::InvalidMessage("expected second placement".to_string()))?;
    let third_key = *placement_keys
        .get(2)
        .ok_or_else(|| Error::InvalidMessage("expected third placement".to_string()))?;
    node.storage.put(&second_key.to_string(), &entry).await?;

    let action = node.entry_lookup(entry.did, 3).await?;
    let evidence = match action {
        PeerRingAction::SomeEntry(evidence) => evidence,
        action => return Err(Error::unexpected_peer_ring_action(action)),
    };
    let repair = node
        .read_repair_entry(evidence.entry.clone(), &evidence.misses, 3)
        .await?;

    assert_eq!(evidence.misses, vec![PlacementMiss::new(
        first_key, node.did
    )]);
    assert_eq!(repair, PeerRingAction::None);
    assert_eq!(
        node.storage.get(&first_key.to_string()).await?,
        Some(entry.clone())
    );
    assert_eq!(
        node.storage.get(&second_key.to_string()).await?,
        Some(entry)
    );
    assert_eq!(node.storage.get(&third_key.to_string()).await?, None);
    Ok(())
}

#[tokio::test]
async fn test_read_repair_uses_observed_remote_owner() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let owner = Did::from(100u32);
    let entry = data_entry(Did::from(10u32));
    let placement_key = *entry
        .did
        .rotate_affine(2)?
        .get(1)
        .ok_or_else(|| Error::InvalidMessage("expected second placement".to_string()))?;

    let action = node
        .read_repair_entry(
            entry.clone(),
            &[PlacementMiss::new(placement_key, owner)],
            2,
        )
        .await?;

    assert_eq!(
        action,
        PeerRingAction::MultiActions(vec![PeerRingAction::RemoteAction(
            owner,
            RemoteAction::SyncEntriesWithSuccessor {
                purpose: StorageSyncPurpose::AdditiveRepair,
                route: StorageSyncRoute::PhysicalOwner,
                data: vec![PlacedEntry::new(placement_key, entry)],
            }
        )])
    );
    Ok(())
}

#[tokio::test]
async fn test_read_repair_rejects_non_affine_observed_miss() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let miss = PlacementMiss::new(non_affine_placement(entry.did, 2)?, node.did);

    let result = node.read_repair_entry(entry, &[miss], 2).await;
    assert!(
        matches!(result, Err(Error::InvalidMessage(message)) if message.contains("affine replica set"))
    );
    Ok(())
}

/// Hand-off law for relay carriers: an element that fails the witness (a carrier written before
/// the storage cutover) is retired by the sender rather than offered, so the receiver can admit
/// and ack the rest instead of skipping the whole carrier on every pass.
#[tokio::test]
async fn test_handoff_retires_an_unwitnessed_inbox_element_and_offers_the_rest() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    // Standing alone, the node routes every position to itself and is the hold authority.
    let node = PeerRing::new_with_storage(holder.delegator_did(), 3, Box::new(MemStorage::new()));
    let new_successor = node.did + Did::from(1u32);
    let destination = node.did + Did::from(100u32);
    let position = inbox_key(destination);
    let key = StorageKey::inbox_of(destination);
    let now_ms = get_epoch_ms();
    let hold =
        EntryOperation::Extend(held_inbox_for(destination, &holder)?).stamped(now_ms, node.did)?;
    node.operate_storage_entry(now_ms, position, hold, node.did)
        .await?;
    // The write law refuses an element that fails the witness, so the junk is stored directly,
    // as a carrier from before the cutover was.
    let junk = Entry::new(position, vec![Bytes::from("junk")], EntryKind::RelayMessage);
    let junk = EntryOperation::Extend(junk).stamped(now_ms, node.did)?;
    let stored = node
        .live_storage_entry(key, now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("hold was not stored".to_string()))?
        .operate(now_ms, junk, node.did)?;
    assert_eq!(stored.data.len(), 2);
    node.storage.put(&key.to_string(), &stored).await?;

    let batches = collect_sync_batches(node.sync_entries_with_successor(new_successor).await?)?;

    let remaining = node
        .live_storage_entry(key, now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("the witnessed hold was retired".to_string()))?;
    assert_eq!(remaining.data.len(), 1);
    remaining.witnessed_inbox_elements(now_ms, node.network_id())?;
    assert_eq!(batches, vec![(new_successor, vec![PlacedEntry::new(
        position, remaining
    )])]);
    Ok(())
}
