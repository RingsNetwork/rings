use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::successor::SuccessorWriter;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;

fn data_entry(did: Did) -> Entry {
    Entry {
        did,
        data: vec![],
        kind: EntryKind::Data,
    }
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

    assert_eq!(
        action,
        PeerRingAction::RemoteAction(
            new_successor,
            RemoteAction::SyncEntriesWithSuccessor(vec![PlacedEntry::new(
                placement_key,
                entry.clone()
            )])
        )
    );
    assert_eq!(retried_action, action);
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
    assert!(matches!(
        action,
        PeerRingAction::RemoteAction(_, RemoteAction::SyncEntriesWithSuccessor(_))
    ));

    let ack_action = node.acknowledge_synced_entries(&[placement_key]).await?;

    assert_eq!(ack_action, PeerRingAction::None);
    assert_eq!(node.storage.get(&placement_key.to_string()).await?, None);
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

    node.acknowledge_synced_entries(&[acked_key]).await?;

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

    node.acknowledge_synced_entries(&[placement_key]).await?;

    assert_eq!(node.storage.get(&placement_key.to_string()).await?, None);
    assert_eq!(
        node.storage.get(&resource_id.to_string()).await?,
        Some(identity_entry)
    );
    Ok(())
}

#[tokio::test]
async fn periodic_republish_restores_missing_local_affine_replica() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));
    let placement_keys = entry.did.rotate_affine(2)?;
    let first_key = placement_keys[0];
    let second_key = placement_keys[1];
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
    let placement_keys = entry.did.rotate_affine(2)?;
    node.storage.put(&entry.did.to_string(), &entry).await?;

    let action = node.republish_local_entries(2).await?;

    assert_eq!(
        action,
        PeerRingAction::MultiActions(vec![
            PeerRingAction::RemoteAction(
                placement_keys[0],
                RemoteAction::SyncEntriesWithSuccessor(vec![PlacedEntry::new(
                    placement_keys[0],
                    entry.clone()
                )])
            ),
            PeerRingAction::RemoteAction(
                placement_keys[1],
                RemoteAction::SyncEntriesWithSuccessor(vec![PlacedEntry::new(
                    placement_keys[1],
                    entry
                )])
            )
        ])
    );
    Ok(())
}

#[tokio::test]
async fn read_repair_is_noop_for_single_replica_storage() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let entry = data_entry(Did::from(10u32));

    let action = node.read_repair_entry(entry, 1).await?;

    assert_eq!(action, PeerRingAction::None);
    assert_eq!(node.storage.count().await?, 0);
    Ok(())
}
