use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;

struct FailingGetStorage;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl KvStorageInterface<Entry> for FailingGetStorage {
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
    let node = PeerRing::new_with_storage(did, 3, Box::new(FailingGetStorage));

    let result = <PeerRing as ChordStorage<_, 1>>::entry_lookup(&node, did).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message == "storage get failed"
    ));
    Ok(())
}

#[tokio::test]
async fn sync_entries_preserves_placement_key_without_deleting_local_copy() -> Result<()> {
    let node_did = Did::from(0u32);
    let new_successor = Did::from(50u32);
    let placement_key = Did::from(100u32);
    let resource_id = Did::from(10u32);
    let entry = Entry {
        did: resource_id,
        data: vec![],
        kind: EntryKind::Data,
    };
    let node = PeerRing::new_with_storage(node_did, 3, Box::new(MemStorage::new()));
    node.storage.put(&placement_key.to_string(), &entry).await?;

    let action = node.sync_entries_with_successor(new_successor).await?;

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
    assert_eq!(
        node.storage.get(&placement_key.to_string()).await?,
        Some(entry)
    );
    Ok(())
}
