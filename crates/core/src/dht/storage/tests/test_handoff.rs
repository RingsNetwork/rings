//! Ownership hand-off to the successor: batching, acknowledgement-gated cleanup, and the
//! shape a relay carrier is offered in.

use super::*;

#[tokio::test]
async fn test_sync_without_ack_retains_entry_for_next_handoff() -> Result<()> {
    let node_did = Did::from(0u32);
    let new_successor = Did::from(50u32);
    let placement_key = Did::from(100u32);
    let resource_id = Did::from(10u32);
    let entry = data_entry(resource_id);
    let node = PeerRing::new_with_storage(node_did, 3, Box::new(MemStorage::new()));
    node.storage.put(&placement_key.to_string(), &entry).await?;

    let action = node.sync_entries_with_successor(new_successor).await?;
    let retried_action = node.sync_entries_with_successor(new_successor).await?;
    let expected = vec![(new_successor, vec![PlacedEntry::new(
        placement_key,
        entry.clone(),
    )])];

    assert_eq!(collect_sync_batches(action)?, expected);
    assert_eq!(collect_sync_batches(retried_action)?, expected);
    assert_eq!(
        node.storage.get(&placement_key.to_string()).await?,
        Some(entry)
    );
    Ok(())
}

#[tokio::test]
async fn test_sync_ack_deletes_local_entry_after_copy() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let new_successor = Did::from(50u32);
    let placement_key = Did::from(100u32);
    let entry = data_entry(Did::from(10u32));
    node.storage.put(&placement_key.to_string(), &entry).await?;

    let action = node.sync_entries_with_successor(new_successor).await?;
    assert_eq!(collect_sync_batches(action)?.len(), 1);

    let ack_action = node
        .acknowledge_synced_entries(&[SyncedEntryAck::new(placement_key, entry)])
        .await?;

    assert_eq!(ack_action, PeerRingAction::None);
    assert_eq!(node.storage.get(&placement_key.to_string()).await?, None);
    Ok(())
}

#[tokio::test]
async fn test_sync_ack_retains_changed_local_value() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let new_successor = Did::from(50u32);
    let placement_key = Did::from(100u32);
    let resource_id = Did::from(10u32);
    let copied_entry = data_entry_with_data(resource_id, "copied");
    let local_write = data_entry_with_data(resource_id, "local-write");
    node.storage
        .put(&placement_key.to_string(), &copied_entry)
        .await?;

    let action = node.sync_entries_with_successor(new_successor).await?;
    assert_eq!(collect_sync_batches(action)?.len(), 1);
    node.storage
        .put(&placement_key.to_string(), &local_write)
        .await?;

    node.acknowledge_synced_entries(&[SyncedEntryAck::new(placement_key, copied_entry)])
        .await?;

    assert_eq!(
        node.storage.get(&placement_key.to_string()).await?,
        Some(local_write)
    );
    Ok(())
}

#[tokio::test]
async fn test_sync_partial_ack_retains_unacked_entries() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let acked_key = Did::from(100u32);
    let pending_key = Did::from(120u32);
    let acked_entry = data_entry(Did::from(10u32));
    let pending_entry = data_entry(Did::from(20u32));
    node.storage
        .put(&acked_key.to_string(), &acked_entry)
        .await?;
    node.storage
        .put(&pending_key.to_string(), &pending_entry)
        .await?;

    node.acknowledge_synced_entries(&[SyncedEntryAck::new(acked_key, acked_entry)])
        .await?;

    assert_eq!(node.storage.get(&acked_key.to_string()).await?, None);
    assert_eq!(
        node.storage.get(&pending_key.to_string()).await?,
        Some(pending_entry)
    );
    Ok(())
}

#[tokio::test]
async fn test_sync_ack_deletes_placement_key_not_entry_identity() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);
    let resource_id = Did::from(10u32);
    let placed_entry = data_entry(resource_id);
    let identity_entry = data_entry(resource_id);
    node.storage
        .put(&placement_key.to_string(), &placed_entry)
        .await?;
    node.storage
        .put(&resource_id.to_string(), &identity_entry)
        .await?;

    node.acknowledge_synced_entries(&[SyncedEntryAck::new(placement_key, placed_entry)])
        .await?;

    assert_eq!(node.storage.get(&placement_key.to_string()).await?, None);
    assert_eq!(
        node.storage.get(&resource_id.to_string()).await?,
        Some(identity_entry)
    );
    Ok(())
}

#[tokio::test]
async fn test_sync_entries_with_successor_batches_by_wire_budget() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let new_successor = Did::from(50u32);
    let payload_len = SYNC_BATCH_MAX_BYTES / 2;
    let entries = vec![
        PlacedEntry::new(
            Did::from(100u32),
            data_entry_with_payload_len(Did::from(10u32), payload_len),
        ),
        PlacedEntry::new(
            Did::from(120u32),
            data_entry_with_payload_len(Did::from(20u32), payload_len),
        ),
        PlacedEntry::new(
            Did::from(140u32),
            data_entry_with_payload_len(Did::from(30u32), payload_len),
        ),
    ];
    for placed in &entries {
        node.storage
            .put(&placed.key.to_string(), &placed.entry)
            .await?;
    }

    let batches = collect_sync_batches(node.sync_entries_with_successor(new_successor).await?)?;

    assert!(
        batches.len() > 1,
        "entries should be split into more than one sync batch"
    );
    for (target, batch) in &batches {
        assert_eq!(*target, new_successor);
        assert!(
            sync_entries_batch_wire_cost(batch)? <= SYNC_BATCH_MAX_BYTES,
            "sync batch exceeds byte budget"
        );
    }
    let actual =
        placed_entries_by_key(batches.into_iter().flat_map(|(_, batch)| batch.into_iter()));
    let expected = placed_entries_by_key(entries);
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn test_sync_entries_batching_emits_oversized_single_entry_alone() -> Result<()> {
    let placed = PlacedEntry::new(
        Did::from(100u32),
        data_entry_with_data(Did::from(10u32), "x"),
    );

    assert!(sync_entries_batch_wire_cost(std::slice::from_ref(&placed))? > 1);
    let batches = sync_entries_batches(vec![placed.clone()], 1)?;

    assert_eq!(batches, vec![vec![placed]]);
    Ok(())
}

#[test]
fn test_sync_entries_batching_preserves_input_order_across_batches() -> Result<()> {
    let entries = vec![
        PlacedEntry::new(
            Did::from(100u32),
            data_entry_with_data(Did::from(10u32), "first"),
        ),
        PlacedEntry::new(
            Did::from(120u32),
            data_entry_with_data(Did::from(20u32), "second"),
        ),
        PlacedEntry::new(
            Did::from(140u32),
            data_entry_with_data(Did::from(30u32), "third"),
        ),
    ];
    let expected_order = entries.iter().map(|placed| placed.key).collect::<Vec<_>>();

    let batches = sync_entries_batches(entries, 1)?;

    assert_eq!(batches.len(), 3);
    let actual_order = batches
        .iter()
        .flat_map(|batch| batch.iter().map(|placed| placed.key))
        .collect::<Vec<_>>();
    assert_eq!(actual_order, expected_order);
    Ok(())
}

#[test]
fn test_sync_entries_batch_wire_cost_matches_serialized_message_cost() -> Result<()> {
    let entries = vec![
        PlacedEntry::new(
            Did::from(100u32),
            data_entry_with_data(Did::from(10u32), "first"),
        ),
        PlacedEntry::new(
            Did::from(120u32),
            data_entry_with_data(Did::from(20u32), "second"),
        ),
    ];
    let message = Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(Did::from(50u32)),
        data: entries.clone(),
    });
    let serialized_bytes = rings_codec::serialized_size(&message).map_err(Error::CodecSerialize)?;
    let message_bytes =
        usize::try_from(serialized_bytes).map_err(|_| Error::MessageSizeOverflow)?;
    let expected = message_bytes
        .checked_add(MAX_PAYLOAD_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD)
        .ok_or(Error::MessageSizeOverflow)?;

    assert_eq!(sync_entries_batch_wire_cost(&entries)?, expected);
    Ok(())
}

#[tokio::test]
async fn test_sync_batch_ack_deletes_acked_batch_and_retries_unacked_batches() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let new_successor = Did::from(50u32);
    let payload_len = SYNC_BATCH_MAX_BYTES / 2;
    let entries = vec![
        PlacedEntry::new(
            Did::from(100u32),
            data_entry_with_payload_len(Did::from(10u32), payload_len),
        ),
        PlacedEntry::new(
            Did::from(120u32),
            data_entry_with_payload_len(Did::from(20u32), payload_len),
        ),
        PlacedEntry::new(
            Did::from(140u32),
            data_entry_with_payload_len(Did::from(30u32), payload_len),
        ),
    ];
    for placed in &entries {
        node.storage
            .put(&placed.key.to_string(), &placed.entry)
            .await?;
    }
    let batches = collect_sync_batches(node.sync_entries_with_successor(new_successor).await?)?;
    let Some((_, acked_batch)) = batches.first() else {
        return Err(Error::InvalidMessage("expected sync batch".to_string()));
    };
    let acked_batch = acked_batch.clone();
    let acks = acked_batch
        .iter()
        .cloned()
        .map(|placed| SyncedEntryAck::new(placed.key, placed.entry))
        .collect::<Vec<_>>();

    node.acknowledge_synced_entries(&acks).await?;

    for placed in &acked_batch {
        assert_eq!(node.storage.get(&placed.key.to_string()).await?, None);
    }
    let retried = collect_sync_batches(node.sync_entries_with_successor(new_successor).await?)?;
    let retried_entries =
        placed_entries_by_key(retried.into_iter().flat_map(|(_, batch)| batch.into_iter()));
    let expected_remaining = placed_entries_by_key(
        entries
            .into_iter()
            .filter(|placed| !acked_batch.iter().any(|acked| acked.key == placed.key)),
    );
    assert_eq!(retried_entries, expected_remaining);
    Ok(())
}

/// Hand-off law for relay carriers: an element that fails the witness for good, and a reset
/// floor (both only in a carrier written before the storage cutover), are retired by the sender
/// rather than offered, so the receiver can admit and ack the rest instead of skipping the whole
/// carrier on every pass.
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
    let mut stored = node
        .live_storage_entry(key, now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("hold was not stored".to_string()))?
        .operate(now_ms, junk, node.did)?;
    assert_eq!(stored.data.len(), 2);
    // A pre-cutover carrier may also hold a reset floor, older than its elements, which no
    // receiver admits on a relay carrier.
    stored.crdt.register = Some(EntryVersion::new(now_ms - 1_000, node.did, node.did));
    node.storage.put(&key.to_string(), &stored).await?;

    let batches = collect_sync_batches(node.sync_entries_with_successor(new_successor).await?)?;

    let remaining = node
        .live_storage_entry(key, now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("the witnessed hold was retired".to_string()))?;
    assert_eq!(remaining.data.len(), 1);
    assert_eq!(remaining.crdt.register, None, "the reset floor is dropped");
    remaining.witnessed_inbox_elements(now_ms, node.network_id())?;
    assert_eq!(batches, vec![(new_successor, vec![PlacedEntry::new(
        position, remaining
    )])]);
    Ok(())
}
