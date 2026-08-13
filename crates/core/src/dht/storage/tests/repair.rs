use super::*;

#[tokio::test]
async fn periodic_republish_restores_missing_local_affine_replica() -> Result<()> {
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
async fn republish_joins_local_branch_and_routes_remote_placement_keys() -> Result<()> {
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
async fn read_repair_is_noop_for_single_replica_storage() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));

    let action = node.read_repair_entry(entry, &[], 1).await?;

    assert_eq!(action, PeerRingAction::None);
    assert_eq!(node.storage.count().await?, 0);
    Ok(())
}

#[tokio::test]
async fn local_hit_lookup_has_no_read_repair_targets() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let mut placement_keys = entry.did.rotate_affine(2)?.into_iter();
    let first_key = placement_keys
        .next()
        .ok_or_else(|| Error::InvalidMessage("expected first placement".to_string()))?;
    node.storage.put(&first_key.to_string(), &entry).await?;

    let action = <PeerRing as ChordStorage<_, 2>>::entry_lookup(&node, entry.did).await?;
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
async fn read_repair_targets_only_observed_missing_placements() -> Result<()> {
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

    let action = <PeerRing as ChordStorage<_, 3>>::entry_lookup(&node, entry.did).await?;
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
async fn read_repair_uses_observed_remote_owner() -> Result<()> {
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
async fn read_repair_rejects_non_affine_observed_miss() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let miss = PlacementMiss::new(non_affine_placement(entry.did, 2)?, node.did);

    let result = node.read_repair_entry(entry, &[miss], 2).await;
    assert!(
        matches!(result, Err(Error::InvalidMessage(message)) if message.contains("affine replica set"))
    );
    Ok(())
}
