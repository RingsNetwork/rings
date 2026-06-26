use std::collections::BTreeMap;

use async_trait::async_trait;

use super::storage_sync::sync_entries_batch_wire_cost;
use super::storage_sync::sync_entries_batches;
use super::storage_sync::SYNC_BATCH_MAX_BYTES;
use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::PlacementMiss;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::successor::SuccessorWriter;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;

fn data_entry(did: Did) -> Entry {
    Entry::new(did, vec![], EntryKind::Data)
}

fn data_entry_with_data(did: Did, data: &str) -> Entry {
    Entry::new(did, vec![data.into()], EntryKind::Data)
}

fn data_entry_with_payload_len(did: Did, len: usize) -> Entry {
    Entry::new(did, vec!["x".repeat(len).into()], EntryKind::Data)
}

fn first_two_affine_keys(did: Did) -> Result<(Did, Did)> {
    let mut keys = did.rotate_affine(2)?.into_iter();
    let Some(first) = keys.next() else {
        return Err(Error::InvalidMessage(
            "rotate_affine(2) returned no placement key".to_string(),
        ));
    };
    let Some(second) = keys.next() else {
        return Err(Error::InvalidMessage(
            "rotate_affine(2) returned one placement key".to_string(),
        ));
    };
    Ok((first, second))
}

fn collect_sync_batches(act: PeerRingAction) -> Result<Vec<(Did, Vec<PlacedEntry>)>> {
    let mut batches = Vec::new();
    collect_sync_batches_into(act, &mut batches)?;
    Ok(batches)
}

fn collect_sync_batches_into(
    act: PeerRingAction,
    batches: &mut Vec<(Did, Vec<PlacedEntry>)>,
) -> Result<()> {
    match act {
        PeerRingAction::None => Ok(()),
        PeerRingAction::RemoteAction(target, RemoteAction::SyncEntriesWithSuccessor(data)) => {
            batches.push((target, data));
            Ok(())
        }
        PeerRingAction::MultiActions(actions) => {
            for action in actions {
                collect_sync_batches_into(action, batches)?;
            }
            Ok(())
        }
        act => Err(Error::unexpected_peer_ring_action(act)),
    }
}

fn placed_entries_by_key(entries: impl IntoIterator<Item = PlacedEntry>) -> BTreeMap<Did, Entry> {
    entries
        .into_iter()
        .map(|placed| (placed.key, placed.entry))
        .collect()
}

struct FailingGetStorageFixture;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl KvStorageInterface<Entry> for FailingGetStorageFixture {
    // Test-only fixture for the read-error boundary. Browser/localStorage
    // adapters are production storage implementations and are cfg-excluded here.
    async fn get(&self, _key: &str) -> Result<Option<Entry>> {
        Err(Error::InvalidMessage("storage get failed".to_string()))
    }

    async fn put(&self, _key: &str, _value: &Entry) -> Result<()> {
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, Entry)>> {
        Ok(vec![])
    }

    async fn remove(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        Ok(0)
    }
}

#[tokio::test]
async fn entry_lookup_reports_local_storage_failure() -> Result<()> {
    let did = Did::from(1u32);
    let node = PeerRing::new_with_storage(did, 3, Box::new(FailingGetStorageFixture));

    let result = <PeerRing as ChordStorage<_, 1>>::entry_lookup(&node, did).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message == "storage get failed"
    ));
    Ok(())
}

#[tokio::test]
async fn sync_without_ack_retains_entry_for_next_handoff() -> Result<()> {
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
async fn sync_ack_deletes_local_entry_after_copy() -> Result<()> {
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
async fn sync_ack_retains_changed_local_value() -> Result<()> {
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
async fn sync_partial_ack_retains_unacked_entries() -> Result<()> {
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
async fn sync_ack_deletes_placement_key_not_entry_identity() -> Result<()> {
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
async fn sync_entries_with_successor_batches_by_wire_budget() -> Result<()> {
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
fn sync_entries_batching_emits_oversized_single_entry_alone() -> Result<()> {
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
fn sync_entries_batching_preserves_input_order_across_batches() -> Result<()> {
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
fn sync_entries_batch_wire_cost_matches_serialized_message_cost() -> Result<()> {
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
        data: entries.clone(),
    });
    let serialized_bytes = bincode::serialized_size(&message).map_err(Error::BincodeSerialize)?;
    let message_bytes =
        usize::try_from(serialized_bytes).map_err(|_| Error::MessageTooLarge(usize::MAX))?;
    let expected = message_bytes
        .checked_add(MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD)
        .ok_or(Error::MessageTooLarge(usize::MAX))?;

    assert_eq!(sync_entries_batch_wire_cost(&entries)?, expected);
    Ok(())
}

#[tokio::test]
async fn sync_batch_ack_deletes_acked_batch_and_retries_unacked_batches() -> Result<()> {
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
async fn republish_remote_actions_preserve_affine_placement_keys() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let successor = Did::from(100u32);
    node.successors().update(successor)?;
    let entry = data_entry(Did::from(10u32));
    let (first_key, second_key) = first_two_affine_keys(entry.did)?;
    node.storage.put(&entry.did.to_string(), &entry).await?;

    let action = node.republish_local_entries(2).await?;

    assert_eq!(
        action,
        PeerRingAction::MultiActions(vec![
            PeerRingAction::RemoteAction(
                first_key,
                RemoteAction::SyncEntriesWithSuccessor(vec![PlacedEntry::new(
                    first_key,
                    entry.clone()
                )])
            ),
            PeerRingAction::RemoteAction(
                second_key,
                RemoteAction::SyncEntriesWithSuccessor(vec![PlacedEntry::new(second_key, entry)])
            )
        ])
    );
    Ok(())
}

#[tokio::test]
async fn read_repair_is_noop_for_single_replica_storage() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));

    let action = node.read_repair_entry(entry, &[]).await?;

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
        .read_repair_entry(evidence.entry.clone(), &evidence.misses)
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
        .read_repair_entry(evidence.entry.clone(), &evidence.misses)
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
    let placement_key = Did::from(40u32);
    let entry = data_entry(Did::from(10u32));

    let action = node
        .read_repair_entry(entry.clone(), &[PlacementMiss::new(placement_key, owner)])
        .await?;

    assert_eq!(
        action,
        PeerRingAction::MultiActions(vec![PeerRingAction::RemoteAction(
            owner,
            RemoteAction::SyncEntriesWithSuccessor(vec![PlacedEntry::new(placement_key, entry)])
        )])
    );
    Ok(())
}
