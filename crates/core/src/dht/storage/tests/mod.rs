use std::collections::BTreeMap;
use std::str::FromStr;

use async_trait::async_trait;
use bytes::Bytes;

use super::super::chord::PeerRing;
use super::super::chord::PeerRingAction;
use super::super::chord::RemoteAction;
use super::sync::SYNC_BATCH_MAX_BYTES;
use crate::delegation::DelegateeKey;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::EntryVersion;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::PlacementMiss;
use crate::dht::stabilization::STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP;
use crate::dht::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::dht::StorageSyncRoute;
use crate::dht::VirtualNodeConfig;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Principal;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::tests::expired;
use crate::tests::held_inbox_for;
use crate::tests::live_entry;
use crate::tests::non_affine_placement;
use crate::tests::volatile_relay_storage;
use crate::utils::get_epoch_ms;

mod test_cache_join;
mod test_cache_linearization;
mod test_handoff;
mod test_horizon;
mod test_own_write;
#[cfg(not(target_family = "wasm"))]
mod test_relay_storage;
mod test_removal_store;
mod test_repair;
mod test_stamp_order;
mod test_tombstone_bound;

fn data_entry(did: Did) -> Entry {
    live_entry(did, vec![], EntryKind::Data)
}

fn data_entry_with_data(did: Did, data: &str) -> Entry {
    live_entry(
        did,
        vec![Bytes::copy_from_slice(data.as_bytes())],
        EntryKind::Data,
    )
}

fn data_entry_with_payload_len(did: Did, len: usize) -> Entry {
    live_entry(did, vec!["x".repeat(len).into()], EntryKind::Data)
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

fn first_virtual_position(node: &PeerRing, owner: Did) -> Result<Did> {
    node.storage_virtual_positions(owner)?
        .into_iter()
        .next()
        .map(|position| position.vnode_did)
        .ok_or_else(|| Error::InvalidMessage("owner has no virtual position".to_string()))
}

fn interval_key_with_virtual_successor(node: &PeerRing, owner: Did) -> Result<Did> {
    let mut positions = node.storage_virtual_positions(node.did)?;
    positions.extend(node.storage_virtual_positions(owner)?);
    positions.sort_by_key(|position| (position.vnode_did, position.owner_did, position.index));

    positions
        .iter()
        .zip(positions.iter().cycle().skip(1))
        .find(|(left, successor)| {
            let key = left.vnode_did + Did::from(1u32);
            successor.owner_did == owner && key != successor.vnode_did
        })
        .map(|(left, _)| left.vnode_did + Did::from(1u32))
        .ok_or_else(|| Error::InvalidMessage("missing virtual successor interval".to_string()))
}

/// The wire deliveries `act` lowers to, as `(purpose, destination, batch)`: the production
/// lowering, so the batch shape is the one the transport sends.
fn collect_sync_messages(
    act: PeerRingAction,
) -> Result<Vec<(StorageSyncPurpose, StorageSyncDestination, Vec<PlacedEntry>)>> {
    Ok(act
        .coalesced_storage_sync_deliveries()?
        .into_iter()
        .map(super::StorageSyncDelivery::into_message_parts)
        .collect())
}

/// The wire batches `act` lowers to, keyed by their destination DID.
fn collect_sync_batches(act: PeerRingAction) -> Result<Vec<(Did, Vec<PlacedEntry>)>> {
    Ok(collect_sync_messages(act)?
        .into_iter()
        .map(|(_, destination, data)| (destination.did(), data))
        .collect())
}

fn placed_entries_by_key(entries: impl IntoIterator<Item = PlacedEntry>) -> BTreeMap<Did, Entry> {
    entries
        .into_iter()
        .map(|placed| (placed.key, placed.entry))
        .collect()
}

#[test]
fn test_coalesced_storage_sync_deliveries_merge_identical_physical_destinations() -> Result<()> {
    let owner = Did::from(50u32);
    let first = PlacedEntry::new(Did::from(100u32), data_entry(Did::from(10u32)));
    let second = PlacedEntry::new(Did::from(120u32), data_entry(Did::from(20u32)));
    let action = PeerRingAction::MultiActions(vec![
        PeerRingAction::sync_entries_for_repair(
            StorageSyncDestination::PhysicalOwner(owner),
            vec![first.clone()],
        ),
        PeerRingAction::sync_entries_for_repair(
            StorageSyncDestination::PhysicalOwner(owner),
            vec![second.clone()],
        ),
    ]);

    let deliveries = action.coalesced_storage_sync_deliveries()?;

    assert_eq!(deliveries.len(), 1);
    let (purpose, destination, data) = deliveries
        .into_iter()
        .next()
        .expect("coalesced delivery")
        .into_message_parts();
    assert_eq!(purpose, StorageSyncPurpose::AdditiveRepair);
    assert_eq!(destination, StorageSyncDestination::PhysicalOwner(owner));
    assert_eq!(data, vec![first, second]);
    Ok(())
}

#[test]
fn test_coalesced_storage_sync_deliveries_keep_placement_destinations_separate() -> Result<()> {
    let first_key = Did::from(100u32);
    let second_key = Did::from(120u32);
    let first = PlacedEntry::new(first_key, data_entry(Did::from(10u32)));
    let second = PlacedEntry::new(second_key, data_entry(Did::from(20u32)));
    let action = PeerRingAction::MultiActions(vec![
        PeerRingAction::sync_entries_for_repair(
            StorageSyncDestination::PlacementKey(first_key),
            vec![first],
        ),
        PeerRingAction::sync_entries_for_repair(
            StorageSyncDestination::PlacementKey(second_key),
            vec![second],
        ),
    ]);

    let deliveries = action.coalesced_storage_sync_deliveries()?;

    assert_eq!(deliveries.len(), 2);
    let destinations = deliveries
        .into_iter()
        .map(|delivery| delivery.into_message_parts().1)
        .collect::<Vec<_>>();
    assert_eq!(destinations, vec![
        StorageSyncDestination::PlacementKey(first_key),
        StorageSyncDestination::PlacementKey(second_key),
    ]);
    Ok(())
}

#[test]
fn test_repair_cursor_identity_ignores_entry_iteration_order() {
    let destination = StorageSyncDestination::PhysicalOwner(Did::from(50u32));
    let first = PlacedEntry::new(Did::from(100u32), data_entry(Did::from(10u32)));
    let second = PlacedEntry::new(Did::from(120u32), data_entry(Did::from(20u32)));
    let forward = super::StorageSyncDelivery::from_parts(
        StorageSyncPurpose::AdditiveRepair,
        destination,
        vec![first.clone(), second.clone()],
    );
    let reverse = super::StorageSyncDelivery::from_parts(
        StorageSyncPurpose::AdditiveRepair,
        destination,
        vec![second, first],
    );

    assert_eq!(forward.cursor_key(), reverse.cursor_key());
}

#[test]
fn test_repair_phase_admits_one_bounded_storage_sync_batch() {
    let frame_bytes = rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

    assert_eq!(SYNC_BATCH_MAX_BYTES, frame_bytes / 4);
    assert_eq!(STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP, 1);
    assert!(SYNC_BATCH_MAX_BYTES <= frame_bytes);
}

struct FailingGetStorageFixture;

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
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
async fn test_entry_lookup_reports_local_storage_failure() -> Result<()> {
    let did = Did::from(1u32);
    let node = PeerRing::new_with_storage(did, 3, Box::new(FailingGetStorageFixture));

    let result = node.entry_lookup(did, 1).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message == "storage get failed"
    ));
    Ok(())
}

#[tokio::test]
async fn test_virtual_storage_owner_routes_operate_to_physical_owner() -> Result<()> {
    let local = Did::from(1u32);
    let remote = Did::from(2u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(remote, None)?;
    let placement = first_virtual_position(&node, remote)?;

    let act = node
        .entry_operate(EntryOperation::Overwrite(data_entry(placement)), 1)
        .await?;

    let PeerRingAction::MultiActions(actions) = act else {
        return Err(Error::unexpected_peer_ring_action(act));
    };
    let Some(PeerRingAction::RemoteAction(target, RemoteAction::FindEntryForOperate(op))) =
        actions.into_iter().next()
    else {
        return Err(Error::InvalidMessage(
            "expected virtual storage operation action".to_string(),
        ));
    };
    {
        assert_eq!(target, remote);
        assert_eq!(op.placement, placement);
        assert_eq!(op.entry_key()?, placement);
    }
    Ok(())
}

#[tokio::test]
async fn test_virtual_storage_owner_routes_interval_key_to_successor_position() -> Result<()> {
    let local = Did::from(1u32);
    let remote = Did::from(2u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(remote, None)?;
    let placement = interval_key_with_virtual_successor(&node, remote)?;

    let act = node
        .entry_operate(EntryOperation::Overwrite(data_entry(placement)), 1)
        .await?;

    let PeerRingAction::MultiActions(actions) = act else {
        return Err(Error::unexpected_peer_ring_action(act));
    };
    let Some(PeerRingAction::RemoteAction(target, RemoteAction::FindEntryForOperate(op))) =
        actions.into_iter().next()
    else {
        return Err(Error::InvalidMessage(
            "expected interval key to route to virtual successor".to_string(),
        ));
    };
    assert_eq!(target, remote);
    assert_eq!(op.placement, placement);
    Ok(())
}

#[tokio::test]
async fn test_virtual_storage_owner_stores_local_position_locally() -> Result<()> {
    let local = Did::from(1u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let placement = first_virtual_position(&node, local)?;

    let act = node
        .entry_operate(EntryOperation::Overwrite(data_entry(placement)), 1)
        .await?;

    assert_eq!(act, PeerRingAction::None);
    assert!(node.storage.get(&placement.to_string()).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_local_fetch_falls_back_when_local_virtual_owner_has_no_entry() -> Result<()> {
    let local = Did::from(1u32);
    let remote = Did::from(2u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(remote, None)?;
    let placement = first_virtual_position(&node, local)?;

    let remote_lookup = node.entry_lookup(placement, 1).await?;
    assert_eq!(
        remote_lookup,
        PeerRingAction::MultiActions(vec![PeerRingAction::EntryMisses(vec![PlacementMiss::new(
            placement, local
        )])])
    );

    let local_fetch_lookup = node.entry_lookup_for_fetch(placement, 1).await?;
    assert_eq!(
        local_fetch_lookup,
        PeerRingAction::MultiActions(vec![PeerRingAction::RemoteAction(
            remote,
            RemoteAction::FindEntry(EntryLookupKey::new(placement, placement)),
        )])
    );
    Ok(())
}

#[tokio::test]
async fn test_virtual_storage_sync_copies_entries_to_observed_virtual_owner() -> Result<()> {
    let local = Did::from(1u32);
    let remote = Did::from(2u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(remote, None)?;
    let placement = first_virtual_position(&node, remote)?;
    let entry = data_entry_with_data(placement, "handoff");
    node.storage.put(&placement.to_string(), &entry).await?;

    let messages =
        collect_sync_messages(node.sync_entries_with_successor(Did::from(99u32)).await?)?;

    assert_eq!(messages.len(), 1);
    let Some((purpose, destination, data)) = messages.into_iter().next() else {
        return Err(Error::InvalidMessage(
            "missing virtual sync batch".to_string(),
        ));
    };
    assert_eq!(purpose, StorageSyncPurpose::AdditiveRepair);
    assert_eq!(destination, StorageSyncDestination::PhysicalOwner(remote));
    assert_eq!(data, vec![PlacedEntry::new(placement, entry)]);
    Ok(())
}

#[test]
fn test_physical_owner_route_permits_a_nonlocal_owner_through_the_current_next_hop() -> Result<()> {
    let local = Did::from(1u32);
    let next_hop = Did::from(10u32);
    let remote_owner = Did::from(5u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(next_hop, None)?;
    let destination = StorageSyncDestination::PhysicalOwner(remote_owner);

    assert_eq!(node.next_hop_for_storage_sync(destination)?, Some(next_hop));
    assert!(node.storage_sync_route_still_permits(destination, next_hop)?);
    Ok(())
}

#[test]
fn test_physical_owner_route_cancels_when_its_direct_next_hop_leaves_topology() -> Result<()> {
    let local = Did::from(1u32);
    let remote_owner = Did::from(2u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(remote_owner, None)?;
    let destination = StorageSyncDestination::PhysicalOwner(remote_owner);

    assert!(node.storage_sync_route_still_permits(destination, remote_owner)?);

    node.remove(remote_owner)?;

    assert!(!node.storage_sync_route_still_permits(destination, remote_owner)?);
    Ok(())
}

#[test]
fn test_physical_owner_route_cancels_when_its_next_hop_changes() -> Result<()> {
    let local = Did::from(1u32);
    let old_next_hop = Did::from(20u32);
    let new_next_hop = Did::from(5u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(old_next_hop, None)?;
    let destination = StorageSyncDestination::PhysicalOwner(old_next_hop);

    assert_eq!(
        node.next_hop_for_storage_sync(destination)?,
        Some(old_next_hop)
    );
    assert!(node.storage_sync_route_still_permits(destination, old_next_hop)?);

    let _ = node.admit_connected(new_next_hop, None)?;

    assert_eq!(
        node.next_hop_for_storage_sync(destination)?,
        Some(new_next_hop)
    );
    assert!(!node.storage_sync_route_still_permits(destination, old_next_hop)?);
    Ok(())
}

#[test]
fn test_placement_key_route_uses_one_virtual_owner_topology_snapshot() -> Result<()> {
    let local = Did::from(1u32);
    let remote = Did::from(10u32);
    let node = PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
        local,
        3,
        Box::new(MemStorage::new()),
        8,
        VirtualNodeConfig::new(7, 2),
    );
    let _ = node.admit_connected(remote, None)?;
    let placement_key = node
        .storage_virtual_positions(remote)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::InvalidMessage("missing remote virtual position".to_string()))?
        .vnode_did;
    let destination = StorageSyncDestination::PlacementKey(placement_key);

    assert_eq!(node.next_hop_for_storage_sync(destination)?, Some(remote));
    assert!(node.storage_sync_route_still_permits(destination, remote)?);
    Ok(())
}

/// Retention law: a stored value whose bound elapsed is retired on read and reported absent.
#[tokio::test]
async fn test_live_storage_read_retires_expired_value() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);
    node.storage
        .put(
            &placement_key.to_string(),
            &expired(data_entry(Did::from(10u32))),
        )
        .await?;

    assert_eq!(
        node.live_storage_entry(
            StorageKey::new(EntryKind::Data, placement_key),
            get_epoch_ms()
        )
        .await?,
        None
    );
    assert_eq!(node.storage.get(&placement_key.to_string()).await?, None);
    Ok(())
}

/// Retention law at the read boundary: a value written to storage without a retention bound (a
/// fixture built with `Entry::new`, or a value from a build that predates retention) is not live,
/// so it is reported absent, removed, and never republished or handed off.
#[tokio::test]
async fn test_live_storage_read_retires_an_unstamped_value() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);
    let unstamped = Entry::new(Did::from(10u32), vec!["value".into()], EntryKind::Data);
    assert_eq!(unstamped.expires_at_ms, None);
    node.storage
        .put(&placement_key.to_string(), &unstamped)
        .await?;

    assert_eq!(
        node.live_storage_entry(
            StorageKey::new(EntryKind::Data, placement_key),
            get_epoch_ms()
        )
        .await?,
        None
    );
    assert_eq!(node.storage.get(&placement_key.to_string()).await?, None);
    assert_eq!(node.republish_local_entries(2).await?, PeerRingAction::None);
    Ok(())
}

/// Retention law: a bulk read returns only live values and retires the rest, so hand-off and
/// republish never offer an expired value.
#[tokio::test]
async fn test_live_storage_entries_retires_expired_values() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let live_key = Did::from(100u32);
    let expired_key = Did::from(200u32);
    let live_value = data_entry(Did::from(10u32));
    node.storage.put(&live_key.to_string(), &live_value).await?;
    node.storage
        .put(
            &expired_key.to_string(),
            &expired(data_entry(Did::from(20u32))),
        )
        .await?;

    let entries = node.live_storage_entries(get_epoch_ms()).await?;
    assert_eq!(entries, vec![(
        StorageKey::new(EntryKind::Data, live_key),
        live_value
    )]);
    assert_eq!(node.storage.count().await?, 1);
    Ok(())
}

/// Key law: the two kinds a position can carry occupy distinct slots, the data slot keeps the
/// historical rendering of a bare placement, and rendering is injective and invertible.
#[test]
fn test_storage_key_partitions_kinds_and_keeps_the_data_rendering() -> Result<()> {
    let placement = Did::from(42u32);
    let topic = StorageKey::new(EntryKind::Data, placement);
    let inbox = StorageKey::new(EntryKind::RelayMessage, placement);

    assert_eq!(topic.to_string(), placement.to_string());
    assert_ne!(topic.to_string(), inbox.to_string());
    assert_eq!(StorageKey::from_str(&topic.to_string())?, topic);
    assert_eq!(StorageKey::from_str(&inbox.to_string())?, inbox);
    assert_eq!(topic.placement(), placement);
    assert_eq!(inbox.placement(), placement);
    Ok(())
}

/// Namespace law at the write funnel: a data topic parked at the inbox position `d + 1` (any
/// node may name any position through an overwrite) and the relay inbox kept for `d` are
/// distinct carriers, so the topic cannot block the hold and the hold cannot touch the topic.
#[tokio::test]
async fn test_data_topic_at_the_inbox_position_does_not_block_the_hold() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    // Standing alone, the node routes every position to itself and is the hold authority.
    let node = PeerRing::new_with_storage(holder.delegator_did(), 3, Box::new(MemStorage::new()))
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(volatile_relay_storage());
    let destination = Did::from(50u32);
    let position = inbox_key(destination);
    let now_ms = get_epoch_ms();

    let park = EntryOperation::Overwrite(data_entry_with_data(position, "parked"))
        .stamped(now_ms, node.did)?;
    node.operate_storage_entry(now_ms, position, park, node.did, Principal::Local)
        .await?;
    let hold =
        EntryOperation::Extend(held_inbox_for(destination, &holder)?).stamped(now_ms, node.did)?;
    node.operate_storage_entry(now_ms, position, hold, node.did, Principal::Local)
        .await?;

    let topic = node
        .live_storage_entry(StorageKey::new(EntryKind::Data, position), now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("parked topic vanished".to_string()))?;
    let inbox = node
        .live_storage_entry(StorageKey::inbox_of(destination), now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("hold was not stored".to_string()))?;
    assert_eq!(topic.kind, EntryKind::Data);
    assert_eq!(topic.data, vec![Bytes::from("parked")]);
    assert_eq!(inbox.kind, EntryKind::RelayMessage);
    assert_eq!(inbox.data.len(), 1);
    // Each namespace has its own store (#1021): the topic and the hold never share one.
    assert_eq!(node.storage.count().await?, 1);
    assert_eq!(
        node.relay_storage
            .as_ref()
            .ok_or(Error::RelayStorageUnavailable)?
            .count()
            .await?,
        1
    );
    Ok(())
}

/// A store whose every access yields to the executor first, so two operations on one slot
/// interleave at each of their storage awaits, the way a hold from the inbound actor and a
/// drain from the stabilizer do.
struct YieldingStorage(MemStorage<Entry>);

#[async_trait]
impl KvStorageInterface<Entry> for YieldingStorage {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        tokio::task::yield_now().await;
        self.0.get(key).await
    }

    async fn put(&self, key: &str, value: &Entry) -> Result<()> {
        tokio::task::yield_now().await;
        self.0.put(key, value).await
    }

    async fn get_all(&self) -> Result<Vec<(String, Entry)>> {
        tokio::task::yield_now().await;
        self.0.get_all().await
    }

    async fn remove(&self, key: &str) -> Result<()> {
        tokio::task::yield_now().await;
        self.0.remove(key).await
    }

    async fn clear(&self) -> Result<()> {
        self.0.clear().await
    }

    async fn count(&self) -> Result<u32> {
        self.0.count().await
    }
}

/// Storage transition law: two operations on one slot that interleave at their storage awaits
/// are serialized, so neither read-modify-write overwrites the other. Here two holds land in
/// one inbox at once, and one hold races the recipient's removal of an earlier element; both
/// holds survive either way.
#[tokio::test]
async fn test_interleaved_operations_on_one_slot_do_not_lose_a_write() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    // The slot is a relay carrier, so the yielding store is the relay store.
    let node = PeerRing::new_with_storage(holder.delegator_did(), 3, Box::new(MemStorage::new()))
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(Box::new(YieldingStorage(MemStorage::new())));
    let destination = Did::from(50u32);
    let position = inbox_key(destination);
    let now_ms = get_epoch_ms();
    let slot = StorageKey::inbox_of(destination);

    let first =
        EntryOperation::Extend(held_inbox_for(destination, &holder)?).stamped(now_ms, node.did)?;
    let second =
        EntryOperation::Extend(held_inbox_for(destination, &holder)?).stamped(now_ms, node.did)?;
    let (first, second) = futures::join!(
        node.operate_storage_entry(now_ms, position, first, node.did, Principal::Local),
        node.operate_storage_entry(now_ms, position, second, node.did, Principal::Local),
    );
    first?;
    second?;
    let inbox = node
        .live_storage_entry(slot, now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("inbox vanished".to_string()))?;
    assert_eq!(inbox.data.len(), 2);

    let first_dot = inbox
        .crdt
        .dots
        .first()
        .copied()
        .ok_or_else(|| Error::InvalidMessage("inbox has no dots".to_string()))?;
    let removal =
        EntryOperation::Tombstone(inbox.removal_of([first_dot])).stamped(now_ms, node.did)?;
    let third =
        EntryOperation::Extend(held_inbox_for(destination, &holder)?).stamped(now_ms, node.did)?;
    let (removal, third) = futures::join!(
        node.operate_storage_entry(now_ms, position, removal, destination, Principal::Unbound),
        node.operate_storage_entry(now_ms, position, third, node.did, Principal::Local),
    );
    removal?;
    third?;
    let inbox = node
        .live_storage_entry(slot, now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("inbox vanished".to_string()))?;
    assert_eq!(inbox.data.len(), 2);
    assert_eq!(inbox.crdt.tombstones.len(), 1);
    Ok(())
}

/// Removal law at the write funnel: a removal never creates a carrier. Against nothing held the
/// result has no retention, so the slot stays empty instead of holding a value the next read
/// would retire; for a data topic by value, for a relay inbox by a dot the recipient once saw.
#[tokio::test]
async fn test_removal_against_nothing_held_stores_nothing() -> Result<()> {
    let holder = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let node = PeerRing::new_with_storage(holder.delegator_did(), 3, Box::new(MemStorage::new()))
        .with_relay_replication(crate::tests::ungated_relay_replication())
        .with_relay_storage(volatile_relay_storage());
    let now_ms = get_epoch_ms();

    let topic = Did::from(100u32);
    let removal = EntryOperation::Tombstone(Entry::new(
        topic,
        vec![Bytes::from("gone")],
        EntryKind::Data,
    ))
    .stamped(now_ms, node.did)?;
    node.operate_storage_entry(now_ms, topic, removal, node.did, Principal::Local)
        .await?;
    assert_eq!(node.storage.count().await?, 0);

    let destination = Did::from(50u32);
    let position = inbox_key(destination);
    let hold =
        EntryOperation::Extend(held_inbox_for(destination, &holder)?).stamped(now_ms, node.did)?;
    node.operate_storage_entry(now_ms, position, hold, node.did, Principal::Local)
        .await?;
    let inbox = node
        .live_storage_entry(StorageKey::inbox_of(destination), now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("hold was not stored".to_string()))?;
    node.storage
        .remove(&StorageKey::inbox_of(destination).to_string())
        .await?;

    let removal = EntryOperation::Tombstone(inbox.removal_of(inbox.crdt.dots.clone()))
        .stamped(now_ms, node.did)?;
    node.operate_storage_entry(now_ms, position, removal, destination, Principal::Unbound)
        .await?;
    assert_eq!(node.storage.count().await?, 0);
    Ok(())
}

/// Admission law at the write funnel: a peer-supplied register at `u128::MAX` is rejected and
/// leaves storage untouched, so it cannot pin the key against honest writers.
#[tokio::test]
async fn test_join_storage_entry_rejects_pinned_register() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);
    let honest = data_entry_with_data(Did::from(10u32), "honest");
    node.join_storage_entry(
        get_epoch_ms(),
        placement_key,
        honest.clone(),
        Principal::Local,
    )
    .await?;

    let mut pinned = data_entry_with_data(Did::from(10u32), "pinned");
    pinned.crdt.register = Some(EntryVersion::new(
        u128::MAX,
        Did::from(99u32),
        Did::from(1u32),
    ));
    assert!(matches!(
        node.join_storage_entry(get_epoch_ms(), placement_key, pinned, Principal::Local)
            .await,
        Err(Error::EntryVersionAheadOfClock)
    ));
    assert_eq!(
        node.storage.get(&placement_key.to_string()).await?,
        Some(honest.try_into_storage_entry()?)
    );
    Ok(())
}

/// Admission law at the write funnel: an expired or over-long retention bound is rejected.
#[tokio::test]
async fn test_join_storage_entry_bounds_retention() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);

    assert!(matches!(
        node.join_storage_entry(
            get_epoch_ms(),
            placement_key,
            expired(data_entry(Did::from(10u32))),
            Principal::Local
        )
        .await,
        Err(Error::EntryNotLive)
    ));
    let mut overlong = data_entry(Did::from(10u32));
    overlong.expires_at_ms = Some(u128::MAX);
    assert!(matches!(
        node.join_storage_entry(get_epoch_ms(), placement_key, overlong, Principal::Local)
            .await,
        Err(Error::EntryLifetimeExceedsMax)
    ));
    assert_eq!(node.storage.count().await?, 0);
    Ok(())
}

/// Retention law under join: an accepted write extends the stored bound to the later of the
/// two, and never shortens it.
#[tokio::test]
async fn test_join_storage_entry_keeps_the_later_retention_bound() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);
    let now_ms = get_epoch_ms();
    let mut earlier = data_entry_with_data(Did::from(10u32), "a");
    earlier.expires_at_ms = Some(now_ms + 1_000);
    let mut later = data_entry_with_data(Did::from(10u32), "b");
    later.expires_at_ms = Some(now_ms + 2_000);

    node.join_storage_entry(now_ms, placement_key, later, Principal::Local)
        .await?;
    let stored = node
        .join_storage_entry(now_ms, placement_key, earlier, Principal::Local)
        .await?;
    assert_eq!(stored.expires_at_ms, Some(now_ms + 2_000));
    Ok(())
}

/// Data topic authorization (#1179): an unprivileged peer cannot wipe an existing data topic
/// written by another peer via `EntryOperation::Overwrite` (either `vec![]` or a single item)
/// or via a synced `register` floor, while a single writer can still overwrite its own entry.
#[tokio::test]
async fn test_operate_storage_entry_rejects_cross_origin_data_overwrite() -> Result<()> {
    let owner = PeerRing::new_with_storage(Did::from(1u32), 3, Box::new(MemStorage::new()));
    let topic = Did::from(100u32);
    let honest = Did::from(10u32);
    let attacker = Did::from(99u32);
    let now_ms = get_epoch_ms();

    let honest_op = owner
        .stamp_operation_by(
            EntryOperation::Extend(data_entry_with_data(topic, "honest-record")),
            now_ms,
            honest,
            Some(topic),
        )
        .await?;
    owner
        .operate_storage_entry(now_ms, topic, honest_op, honest, Principal::Unbound)
        .await?;

    // 1. Remote empty Overwrite (`vec![]`) is rejected.
    let empty_wipe = owner
        .stamp_operation_by(
            EntryOperation::Overwrite(live_entry(topic, vec![], EntryKind::Data)),
            now_ms + 1,
            attacker,
            Some(topic),
        )
        .await?;
    assert!(matches!(
        owner
            .operate_storage_entry(now_ms + 1, topic, empty_wipe, attacker, Principal::Unbound)
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 2. Single-item Overwrite from another peer against honest writer's live entry is rejected.
    let hijack_overwrite = owner
        .stamp_operation_by(
            EntryOperation::Overwrite(data_entry_with_data(topic, "attacker-record")),
            now_ms + 2,
            attacker,
            Some(topic),
        )
        .await?;
    assert!(matches!(
        owner
            .operate_storage_entry(
                now_ms + 2,
                topic,
                hijack_overwrite,
                attacker,
                Principal::Unbound,
            )
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 3. Synced entry forging a synthetic floor claiming `actor = honest` is rejected (#1220),
    //    while a legitimate cross-actor overwrite floor on another replica merges commutatively
    //    without wiping `honest-record`.
    let mut forged_sync = owner
        .stamp_operation_by(
            EntryOperation::Overwrite(data_entry_with_data(topic, "attacker-sync")),
            now_ms + 3,
            attacker,
            Some(topic),
        )
        .await?
        .entry()
        .clone();
    let forged_floor = crate::dht::entry::EntryVersion::new(now_ms + 3, honest, Did::from(9999u32));
    forged_sync.crdt.register = Some(forged_floor);
    forged_sync.crdt.dots[0].version = forged_floor;
    assert!(matches!(
        owner
            .join_storage_entry(now_ms + 3, topic, forged_sync, Principal::Unbound)
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    let stored = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 4)
        .await?
        .ok_or_else(|| Error::InvalidMessage("honest topic vanished".to_string()))?;
    assert_eq!(stored.data, vec![Bytes::from("honest-record")]);

    // 4. The original single writer can still overwrite its own entry.
    let self_overwrite = owner
        .stamp_operation_by(
            EntryOperation::Overwrite(data_entry_with_data(topic, "honest-updated")),
            now_ms + 5,
            honest,
            Some(topic),
        )
        .await?;
    owner
        .operate_storage_entry(
            now_ms + 5,
            topic,
            self_overwrite,
            honest,
            Principal::Unbound,
        )
        .await?;
    let updated = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 6)
        .await?
        .ok_or_else(|| Error::InvalidMessage("updated topic vanished".to_string()))?;
    assert_eq!(updated.data, vec![Bytes::from("honest-updated")]);
    Ok(())
}

/// Data topic authorization (#1179): a peer cannot tombstone another peer's data entry (either
/// by value witness or dot witness, or via forged synced tombstones), while the original writer
/// can tombstone its own entry.
#[tokio::test]
async fn test_operate_storage_entry_rejects_cross_origin_data_tombstone() -> Result<()> {
    let owner = PeerRing::new_with_storage(Did::from(1u32), 3, Box::new(MemStorage::new()));
    let topic = Did::from(100u32);
    let honest = Did::from(10u32);
    let attacker = Did::from(99u32);
    let now_ms = get_epoch_ms();

    let honest_op = owner
        .stamp_operation_by(
            EntryOperation::Extend(data_entry_with_data(topic, "honest-record")),
            now_ms,
            honest,
            Some(topic),
        )
        .await?;
    owner
        .operate_storage_entry(now_ms, topic, honest_op, honest, Principal::Unbound)
        .await?;
    let stored = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("honest topic missing".to_string()))?;
    let honest_dot = stored.crdt.dots[0];

    // 1. Cross-origin Tombstone by value witness is rejected.
    let value_tombstone = owner
        .stamp_operation_by(
            EntryOperation::Tombstone(data_entry_with_data(topic, "honest-record")),
            now_ms + 1,
            attacker,
            Some(topic),
        )
        .await?;
    assert!(matches!(
        owner
            .operate_storage_entry(
                now_ms + 1,
                topic,
                value_tombstone,
                attacker,
                Principal::Unbound,
            )
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 2. Cross-origin Tombstone by dot witness is rejected.
    let mut dot_removal = Entry::new(topic, vec![], EntryKind::Data);
    dot_removal.crdt.dots = vec![honest_dot];
    dot_removal.expires_at_ms = stored.expires_at_ms;
    let dot_tombstone = EntryOperation::Tombstone(dot_removal);
    assert!(matches!(
        owner
            .operate_storage_entry(
                now_ms + 2,
                topic,
                dot_tombstone,
                attacker,
                Principal::Unbound,
            )
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 3. Synced tombstone impersonating `actor = honest` with an unauthenticated version is
    //    rejected (#1220).
    let mut forged_sync = owner
        .stamp_operation_by(
            EntryOperation::Extend(data_entry_with_data(topic, "attacker-item")),
            now_ms + 3,
            attacker,
            Some(topic),
        )
        .await?
        .entry()
        .clone();
    let forged_honest_dot = crate::dht::entry::EntryDot {
        version: EntryVersion::new(now_ms + 3, honest, Did::from(9999u32)),
        index: 0,
    };
    forged_sync
        .crdt
        .tombstones
        .push(crate::dht::entry::EntryTombstone::of(
            &Bytes::from("honest-record"),
            forged_honest_dot,
        ));
    assert!(matches!(
        owner
            .join_storage_entry(now_ms + 3, topic, forged_sync, Principal::Unbound)
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    let still_live = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 4)
        .await?
        .ok_or_else(|| Error::InvalidMessage("honest topic vanished".to_string()))?;
    assert_eq!(still_live.data, vec![Bytes::from("honest-record")]);

    // 4. The original writer can tombstone its own entry.
    let self_tombstone = owner
        .stamp_operation_by(
            EntryOperation::Tombstone(data_entry_with_data(topic, "honest-record")),
            now_ms + 5,
            honest,
            Some(topic),
        )
        .await?;
    owner
        .operate_storage_entry(
            now_ms + 5,
            topic,
            self_tombstone,
            honest,
            Principal::Unbound,
        )
        .await?;
    let after_self_tombstone = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 6)
        .await?
        .ok_or_else(|| Error::InvalidMessage("carrier vanished".to_string()))?;
    assert!(after_self_tombstone.data.is_empty());

    // 5. Out-of-order pre-planted tombstone on an empty topic (`local == None`) cannot cover a
    //    subsequent honest write or sync with an earlier logical timestamp.
    let fresh_topic = Did::from(101u32);
    let mut preplanted_sync = owner
        .stamp_operation_by(
            EntryOperation::Extend(data_entry_with_data(fresh_topic, "attacker-seed")),
            now_ms + 100,
            attacker,
            Some(fresh_topic),
        )
        .await?
        .entry()
        .clone();
    let preplanted_dot = preplanted_sync.crdt.dots[0];
    preplanted_sync
        .crdt
        .tombstones
        .push(crate::dht::entry::EntryTombstone::of(
            &Bytes::from("honest-late-write"),
            preplanted_dot,
        ));
    preplanted_sync
        .crdt
        .tombstones
        .push(crate::dht::entry::EntryTombstone::of(
            &Bytes::from("honest-late-sync"),
            preplanted_dot,
        ));
    owner
        .join_storage_entry(
            now_ms + 100,
            fresh_topic,
            preplanted_sync,
            Principal::Unbound,
        )
        .await?;

    let mut honest_earlier_write = data_entry_with_data(fresh_topic, "honest-late-write");
    honest_earlier_write.crdt.dots = vec![crate::dht::entry::EntryDot {
        version: EntryVersion::new(now_ms + 50, honest, honest),
        index: 0,
    }];
    owner
        .operate_storage_entry(
            now_ms + 101,
            fresh_topic,
            EntryOperation::Extend(honest_earlier_write),
            honest,
            Principal::Unbound,
        )
        .await?;

    let mut honest_earlier_sync = data_entry_with_data(fresh_topic, "honest-late-sync");
    honest_earlier_sync.crdt.dots = vec![crate::dht::entry::EntryDot {
        version: EntryVersion::new(now_ms + 50, honest, honest),
        index: 0,
    }];
    owner
        .join_storage_entry(
            now_ms + 102,
            fresh_topic,
            honest_earlier_sync,
            Principal::Unbound,
        )
        .await?;

    let fresh_stored = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, fresh_topic), now_ms + 103)
        .await?
        .ok_or_else(|| Error::InvalidMessage("fresh topic vanished".to_string()))?;
    assert!(
        fresh_stored
            .data
            .contains(&Bytes::from("honest-late-write")),
        "pre-planted attacker tombstone must not cover honest writer's operate_storage_entry"
    );
    assert!(
        fresh_stored.data.contains(&Bytes::from("honest-late-sync")),
        "pre-planted attacker tombstone must not cover honest writer's join_storage_entry"
    );
    Ok(())
}

/// Bounded per-origin Extend on shared topics (#1179): one writer flooding `ENTRY_DATA_MAX_LEN`
/// items into a shared topic via `Extend` cannot evict other writers' entries.
#[tokio::test]
async fn test_operate_storage_entry_bounded_per_origin_extend_preserves_other_writers() -> Result<()>
{
    let owner = PeerRing::new_with_storage(Did::from(1u32), 3, Box::new(MemStorage::new()));
    let topic = Did::from(100u32);
    let honest_a = Did::from(10u32);
    let honest_b = Did::from(20u32);
    let flooder = Did::from(99u32);
    let now_ms = get_epoch_ms();

    for (idx, (writer, label)) in [(honest_a, "honest-a"), (honest_b, "honest-b")]
        .into_iter()
        .enumerate()
    {
        let op = owner
            .stamp_operation_by(
                EntryOperation::Extend(data_entry_with_data(topic, label)),
                now_ms + idx as u128,
                writer,
                Some(topic),
            )
            .await?;
        owner
            .operate_storage_entry(now_ms + idx as u128, topic, op, writer, Principal::Unbound)
            .await?;
    }

    let flood_items = (0..crate::consts::ENTRY_DATA_MAX_LEN)
        .map(|i| Bytes::from(format!("flood-{i}")))
        .collect::<Vec<_>>();
    let flood_entry = live_entry(topic, flood_items, EntryKind::Data);
    let flood_op = owner
        .stamp_operation_by(
            EntryOperation::Extend(flood_entry),
            now_ms + 10,
            flooder,
            Some(topic),
        )
        .await?;
    owner
        .operate_storage_entry(now_ms + 10, topic, flood_op, flooder, Principal::Unbound)
        .await?;

    let stored = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 11)
        .await?
        .ok_or_else(|| Error::InvalidMessage("shared topic vanished".to_string()))?;
    assert_eq!(stored.data.len(), crate::consts::ENTRY_DATA_MAX_LEN);
    assert!(
        stored.data.contains(&Bytes::from("honest-a")),
        "honest_a must survive the flood"
    );
    assert!(
        stored.data.contains(&Bytes::from("honest-b")),
        "honest_b must survive the flood"
    );
    Ok(())
}

/// Issue #1220 Defect 2: actor-scoped `co_registers` and `co_removes` make bidirectional storage
/// sync unconditionally commutative (no `OwnershipHandoff` refusal or cross-actor data loss) and
/// survive element-side eviction in `CarrierStore::Factored` (`RemovalRecord`).
#[tokio::test]
async fn test_issue_1220_actor_scoped_floors_and_tombstones_commute_and_survive_factored_eviction(
) -> Result<()> {
    let owner_1 = PeerRing::new_with_storage(Did::from(1u32), 3, Box::new(MemStorage::new()));
    let owner_2 = PeerRing::new_with_storage(Did::from(2u32), 3, Box::new(MemStorage::new()));
    let topic = Did::from(1220u32);
    let actor_a = Did::from(10u32);
    let actor_b = Did::from(20u32);
    let actor_c = Did::from(30u32);
    let now_ms = get_epoch_ms();

    // owner_1: actor_a overwrites "a-v1" at t+10 then "a-v2" at t+200 (floor_a = t+200).
    for (offset, val) in [(10u128, "a-v1"), (200u128, "a-v2")] {
        let op = owner_1
            .stamp_operation_by(
                EntryOperation::Overwrite(data_entry_with_data(topic, val)),
                now_ms + offset,
                actor_a,
                Some(topic),
            )
            .await?;
        owner_1
            .operate_storage_entry(now_ms + offset, topic, op, actor_a, Principal::Unbound)
            .await?;
    }

    // owner_2: actor_c overwrites "c-v1" at t+180 (floor_c = t+180), then actor_b extends "b-v1"
    // at t+150 (an older timestamp from a different actor!).
    let op_c = owner_2
        .stamp_operation_by(
            EntryOperation::Overwrite(data_entry_with_data(topic, "c-v1")),
            now_ms + 180,
            actor_c,
            Some(topic),
        )
        .await?;
    owner_2
        .operate_storage_entry(now_ms + 180, topic, op_c, actor_c, Principal::Unbound)
        .await?;

    let mut b_delta = data_entry_with_data(topic, "b-v1");
    b_delta.crdt.dots = vec![crate::dht::entry::EntryDot {
        version: EntryVersion::new(now_ms + 150, actor_b, actor_b),
        index: 0,
    }];
    owner_2
        .join_storage_entry(now_ms + 181, topic, b_delta, Principal::Unbound)
        .await?;

    let state_1 = owner_1
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 201)
        .await?
        .ok_or_else(|| Error::InvalidMessage("state_1 missing".to_string()))?;
    let state_2 = owner_2
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 201)
        .await?
        .ok_or_else(|| Error::InvalidMessage("state_2 missing".to_string()))?;

    // Bidirectional OwnershipHandoff / SyncEntriesWithSuccessor succeeds in BOTH orders and
    // converges to the exact same state!
    let merged_on_1 = owner_1
        .join_storage_entry(now_ms + 202, topic, state_2, Principal::Unbound)
        .await?;
    let merged_on_2 = owner_2
        .join_storage_entry(now_ms + 202, topic, state_1, Principal::Unbound)
        .await?;
    assert_eq!(merged_on_1, merged_on_2);
    assert!(merged_on_1.data.contains(&Bytes::from("a-v2")));
    assert!(merged_on_1.data.contains(&Bytes::from("b-v1")));
    assert!(merged_on_1.data.contains(&Bytes::from("c-v1")));
    assert!(!merged_on_1.data.contains(&Bytes::from("a-v1")));
    assert_eq!(merged_on_1.crdt.co_registers.len(), 1);

    // Wire round-trip (bincode/rings_codec and JSON) preserves both `register` and `co_registers`.
    let wire_bytes = rings_codec::serialize(&merged_on_1).map_err(Error::CodecSerialize)?;
    let wire_decoded: Entry =
        rings_codec::deserialize(&wire_bytes).map_err(Error::CodecDeserialize)?;
    assert_eq!(wire_decoded, merged_on_1);

    // Simulate byte-budget eviction of the element side in `CarrierStore::Factored`: both
    // `register` (`actor_a`) and `co_registers` (`actor_c`) remain in `RemovalRecord` and suppress
    // stale replays from `actor_a` and `actor_c` while admitting `actor_b`!
    owner_1.storage.remove(&topic.to_string()).await?;
    let stale_c = crate::tests::stamped_delta(
        EntryOperation::Extend,
        topic,
        &["c-stale"],
        now_ms + 50,
        actor_c,
    )?;
    let stale_a = crate::tests::stamped_delta(
        EntryOperation::Extend,
        topic,
        &["a-stale"],
        now_ms + 50,
        actor_a,
    )?;
    let fresh_b = crate::tests::stamped_delta(
        EntryOperation::Extend,
        topic,
        &["b-survives"],
        now_ms + 60,
        actor_b,
    )?;
    owner_1
        .join_storage_entry(now_ms + 205, topic, stale_c, Principal::Unbound)
        .await?;
    owner_1
        .join_storage_entry(now_ms + 205, topic, stale_a, Principal::Unbound)
        .await?;
    let after_eviction = owner_1
        .join_storage_entry(now_ms + 205, topic, fresh_b, Principal::Unbound)
        .await?;
    assert_eq!(after_eviction.data, vec![Bytes::from("b-survives")]);
    Ok(())
}

/// Issue #1220 Defects 3 & 5b: `SignedDataMutation` cryptographically authenticates Swarm-originated
/// per-actor mutations across `SyncEntriesWithSuccessor` (`join_storage_entry`) and `FoundEntry`
/// (`local_cache_put` on `CarrierStore::Whole`), rejecting forged `held_dot` tombstones,
/// cross-element dot-retraction replays, forged `OperationDigest` floors, and corrupted wire proofs.
#[tokio::test]
async fn test_issue_1220_signed_data_mutation_authenticates_swarm_sync_and_local_cache(
) -> Result<()> {
    use crate::dht::ChordStorageCache;
    use crate::message::MessageSigner;
    use crate::tests::TEST_NETWORK_ID;

    let owner = PeerRing::new_with_storage(Did::from(1u32), 3, Box::new(MemStorage::new()));
    let reader = PeerRing::new_with_storage(Did::from(2u32), 3, Box::new(MemStorage::new()));
    let replica = PeerRing::new_with_storage(Did::from(3u32), 3, Box::new(MemStorage::new()));

    let victim_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let attacker_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let victim = victim_key.delegator_did();
    let attacker = attacker_key.delegator_did();
    let victim_signer = MessageSigner::new(&victim_key, TEST_NETWORK_ID);
    let attacker_signer = MessageSigner::new(&attacker_key, TEST_NETWORK_ID);

    let topic = Did::from(1221u32);
    let now_ms = get_epoch_ms();

    // Victim issues two Swarm-signed Extend operations ("v-keep" and "v-other").
    for (offset, label) in [(0u128, "v-keep"), (1u128, "v-other")] {
        let op = owner
            .stamp_operation_by(
                EntryOperation::Extend(data_entry_with_data(topic, label)),
                now_ms + offset,
                victim,
                Some(topic),
            )
            .await?
            .with_author_proofs(victim, victim_signer, now_ms + offset)?;
        owner
            .operate_storage_entry(
                now_ms + offset,
                topic,
                op.clone(),
                victim,
                Principal::Unbound,
            )
            .await?;
        replica
            .operate_storage_entry(now_ms + offset, topic, op, victim, Principal::Unbound)
            .await?;
    }

    let held = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 2)
        .await?
        .ok_or_else(|| Error::InvalidMessage("victim topic missing".to_string()))?;
    assert_eq!(held.crdt.proofs.len(), 2);
    reader.local_cache_put(held.clone()).await?;

    let keep_dot = held.crdt.dots[0];
    let other_dot = held.crdt.dots[1];

    // 1. Forged synced tombstone copying `victim`'s exact `keep_dot` (without `victim`'s signature,
    //    or signed by `attacker_signer`) is rejected on BOTH replicated storage and `Whole` cache!
    let mut forged_copied_dot = replica
        .stamp_operation_by(
            EntryOperation::Extend(data_entry_with_data(topic, "attacker-noise")),
            now_ms + 5,
            attacker,
            Some(topic),
        )
        .await?
        .with_author_proofs(attacker, attacker_signer, now_ms + 5)?
        .entry()
        .clone();
    forged_copied_dot
        .crdt
        .tombstones
        .push(crate::dht::entry::EntryTombstone::of(
            &Bytes::from("v-keep"),
            keep_dot,
        ));
    assert!(matches!(
        owner
            .join_storage_entry(
                now_ms + 5,
                topic,
                forged_copied_dot.clone(),
                Principal::Unbound
            )
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));
    assert!(matches!(
        reader.local_cache_put(forged_copied_dot).await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 2. Cross-element dot-retraction replay: victim legitimately retracts `other_dot` ("v-other")
    //    by dot witness on `replica`, and attacker transplants that signature onto `keep_dot`
    //    ("v-keep"). Must be rejected!
    let mut dot_removal = Entry::new(topic, vec![], EntryKind::Data);
    dot_removal.crdt.dots = vec![other_dot];
    let legit_other_tomb = EntryOperation::Tombstone(dot_removal)
        .stamped(now_ms + 6, victim)?
        .with_author_proofs(victim, victim_signer, now_ms + 6)?;
    replica
        .operate_storage_entry(
            now_ms + 6,
            topic,
            legit_other_tomb,
            victim,
            Principal::Unbound,
        )
        .await?;
    let replica_after_other_tomb = replica
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 6)
        .await?
        .ok_or_else(|| Error::InvalidMessage("replica missing".to_string()))?;

    let mut replayed_proof_attack = replica_after_other_tomb.clone();
    replayed_proof_attack.data.clear();
    replayed_proof_attack.crdt.dots.clear();
    replayed_proof_attack.crdt.tombstones = vec![crate::dht::entry::EntryTombstone::of(
        &Bytes::from("v-keep"),
        keep_dot,
    )];
    if let Some(proof) = replayed_proof_attack.crdt.proofs.first_mut() {
        proof.key = crate::dht::entry::DataProofKey::Remove(
            crate::dht::entry::ElementDigest::of(&Bytes::from("v-keep")),
            victim,
        );
    }
    assert!(matches!(
        owner
            .join_storage_entry(now_ms + 7, topic, replayed_proof_attack, Principal::Unbound)
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 3. Legitimate sync of `replica_after_other_tomb` succeeds on both `owner` and `reader` cache!
    owner
        .join_storage_entry(
            now_ms + 8,
            topic,
            replica_after_other_tomb.clone(),
            Principal::Unbound,
        )
        .await?;
    reader.local_cache_put(replica_after_other_tomb).await?;
    let owner_after = owner
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 8)
        .await?
        .ok_or_else(|| Error::InvalidMessage("owner_after missing".to_string()))?;
    assert_eq!(owner_after.data, vec![Bytes::from("v-keep")]);

    // 4. Forged synced `Overwrite` claiming `actor = victim` with valid `OperationDigest` but
    //    lacking `victim`'s `SignedDataMutation` is rejected!
    let unsigned_victim_overwrite = owner
        .stamp_operation_by(
            EntryOperation::Overwrite(data_entry_with_data(topic, "forged-reset")),
            now_ms + 10,
            victim,
            Some(topic),
        )
        .await?
        .entry()
        .clone();
    assert!(matches!(
        owner
            .join_storage_entry(
                now_ms + 10,
                topic,
                unsigned_victim_overwrite,
                Principal::Unbound
            )
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // 5. Legitimate Swarm-signed `Overwrite` by `victim` syncs cleanly and wire-roundtrips;
    //    truncating a proof chunk on the wire is rejected as corrupted.
    let signed_victim_overwrite = replica
        .stamp_operation_by(
            EntryOperation::Overwrite(data_entry_with_data(topic, "v-reset")),
            now_ms + 12,
            victim,
            Some(topic),
        )
        .await?
        .with_author_proofs(victim, victim_signer, now_ms + 12)?;
    replica
        .operate_storage_entry(
            now_ms + 12,
            topic,
            signed_victim_overwrite,
            victim,
            Principal::Unbound,
        )
        .await?;
    let signed_reset_state = replica
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms + 12)
        .await?
        .ok_or_else(|| Error::InvalidMessage("signed_reset_state missing".to_string()))?;

    let mut wire_json =
        serde_json::to_value(&signed_reset_state).map_err(|_| Error::SerializeToString)?;
    if let Some(dots_arr) = wire_json
        .get_mut("crdt")
        .and_then(|crdt| crdt.get_mut("dots"))
        .and_then(|dots| dots.as_array_mut())
    {
        dots_arr.pop();
    }
    let truncated_entry: Entry = serde_json::from_value(wire_json).map_err(Error::Deserialize)?;
    assert!(matches!(
        owner
            .join_storage_entry(now_ms + 13, topic, truncated_entry, Principal::Unbound)
            .await,
        Err(Error::DataEntryWriteUnauthorized)
    ));

    let final_owner = owner
        .join_storage_entry(now_ms + 14, topic, signed_reset_state, Principal::Unbound)
        .await?;
    assert_eq!(final_owner.data, vec![Bytes::from("v-reset")]);
    Ok(())
}
