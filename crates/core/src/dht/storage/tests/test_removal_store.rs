//! The factored data store (#920; removal-stability paper, Laws 2–4 and 6): eviction drops
//! element sides and never a remove, the removal store is shared by origin, and the recovery
//! pass normalizes an element side a crash rolled back.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::oneshot;
use rings_test_support::with_hang_guard;

use super::super::super::chord::PeerRing;
use crate::consts::DATA_TOMBSTONE_MAX_LEN;
use crate::consts::REMOVAL_ORIGIN_SHARE;
use crate::consts::REMOVAL_STORE_MAX_REMOVES;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::ChargedRemove;
use crate::dht::Did;
use crate::dht::RemovalCharge;
use crate::dht::RemovalRecord;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::tests::removes_only_carrier;
use crate::tests::stamped_delta;
use crate::utils::get_epoch_ms;

/// The data carrier stored at `did` on `node`, as a read under the storage transition sees it.
async fn stored(node: &PeerRing, did: Did, now_ms: u128) -> Result<Option<Entry>> {
    node.live_storage_entry(StorageKey::new(EntryKind::Data, did), now_ms)
        .await
}

/// A node whose data store holds one element side, so every new key evicts the last one.
fn evicting_node() -> PeerRing {
    let one = NonZeroU32::MIN;
    PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::bounded(one)))
}

/// The trace of #920 cannot occur: eviction drops the topic's element side, its remove survives
/// in the removal store, and a stale replica's sync of the removed element is covered by it.
#[tokio::test]
async fn test_eviction_keeps_the_remove_a_stale_sync_would_undo() -> Result<()> {
    let node = evicting_node();
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let writer = Did::from(7u32);
    let added = stamped_delta(EntryOperation::Extend, topic, &["a"], now_ms, writer)?;
    let removed = added.tombstone(added.clone())?;
    node.join_storage_entry(now_ms, topic, removed, writer)
        .await?;

    let filler = stamped_delta(
        EntryOperation::Extend,
        Did::from(11u32),
        &["b"],
        now_ms,
        writer,
    )?;
    node.join_storage_entry(now_ms, Did::from(11u32), filler, writer)
        .await?;
    assert_eq!(node.storage.get(&topic.to_string()).await?, None, "evicted");

    node.join_storage_entry(now_ms, topic, added, Did::from(8u32))
        .await?;
    let read = stored(&node, topic, now_ms).await?;
    assert_eq!(read.as_ref().map(|entry| entry.data.len()), Some(0));
    assert_eq!(read.map(|entry| entry.crdt.tombstones.len()), Some(1));
    Ok(())
}

/// Shares (Law 6, Prop. 6): an origin that holds its share is refused a further remove on any
/// topic, which changes nothing, while another origin and the holder class still remove.
#[tokio::test]
async fn test_an_origin_past_its_share_is_refused_and_others_are_not() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let flooder = Did::from(70u32);
    assert_eq!(REMOVAL_ORIGIN_SHARE, DATA_TOMBSTONE_MAX_LEN);
    node.join_storage_entry(
        now_ms,
        Did::from(10u32),
        removes_only_carrier(Did::from(10u32), REMOVAL_ORIGIN_SHARE, now_ms, "share")?,
        flooder,
    )
    .await?;

    let other_topic = Did::from(11u32);
    let one_more = removes_only_carrier(other_topic, 1, now_ms, "more")?;
    assert!(matches!(
        node.join_storage_entry(now_ms, other_topic, one_more.clone(), flooder)
            .await,
        Err(Error::RemovalCapacityExhausted { origin: Some(origin), .. }) if origin == flooder
    ));
    assert_eq!(stored(&node, other_topic, now_ms).await?, None);

    node.join_storage_entry(now_ms, other_topic, one_more, Did::from(71u32))
        .await?;
    node.join_storage_entry(
        now_ms,
        Did::from(12u32),
        removes_only_carrier(Did::from(12u32), 1, now_ms, "holder")?,
        node.did,
    )
    .await?;
    let charges = node
        .removal_storage
        .get(&other_topic.to_string())
        .await?
        .map(|record| {
            record
                .removes
                .iter()
                .map(|remove| remove.charge)
                .collect::<Vec<_>>()
        });
    assert_eq!(charges, Some(vec![RemovalCharge::Origin(Did::from(71u32))]));
    Ok(())
}

/// The store's total binds only once `B_R / S` origins each hold their share: the next origin's
/// remove is refused for the total, not for its own share.
#[tokio::test]
async fn test_the_removal_store_refuses_past_its_total() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let origins = u32::try_from(REMOVAL_STORE_MAX_REMOVES / REMOVAL_ORIGIN_SHARE)
        .map_err(|_| Error::MessageSizeOverflow)?;
    for origin in 0..origins {
        let topic = Did::from(1_000 + origin);
        node.join_storage_entry(
            now_ms,
            topic,
            removes_only_carrier(topic, REMOVAL_ORIGIN_SHARE, now_ms, "fill")?,
            Did::from(100_000 + origin),
        )
        .await?;
    }

    let topic = Did::from(9u32);
    assert!(matches!(
        node.join_storage_entry(
            now_ms,
            topic,
            removes_only_carrier(topic, 1, now_ms, "late")?,
            Did::from(99u32),
        )
        .await,
        Err(Error::RemovalCapacityExhausted { origin: None, bound, .. })
            if bound == REMOVAL_STORE_MAX_REMOVES
    ));
    Ok(())
}

/// Recovery (Law 4, Prop. 2): an element side a crash rolled back to hold an add its remove side
/// covers is normalized by the first operation on the store, so the add is never served.
#[tokio::test]
async fn test_recovery_normalizes_a_rolled_back_element_side() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let writer = Did::from(7u32);
    let added = stamped_delta(EntryOperation::Extend, topic, &["a"], now_ms, writer)?
        .try_into_storage_entry()?;
    let removed = added.tombstone(added.clone())?;
    let record = RemovalRecord {
        did: topic,
        register: None,
        removes: removed
            .crdt
            .tombstones
            .iter()
            .map(|tombstone| ChargedRemove {
                tombstone: *tombstone,
                charge: RemovalCharge::Origin(writer),
            })
            .collect(),
    };
    node.removal_storage
        .put(&topic.to_string(), &record)
        .await?;
    node.storage.put(&topic.to_string(), &added).await?;

    let read = stored(&node, topic, now_ms).await?;
    assert_eq!(read.map(|entry| entry.data), Some(Vec::<Bytes>::new()));
    assert_eq!(
        node.storage
            .get(&topic.to_string())
            .await?
            .map(|side| side.data.len()),
        Some(0),
        "the recovery pass rewrote the element side"
    );
    Ok(())
}

/// A removal store whose next `put` parks after it is called, until released, so a test can hold
/// a remove-side write mid-flush.
struct ParkingRemovals {
    /// The store the operations reach.
    inner: MemStorage<RemovalRecord>,
    /// The parked signal and the release of the next `put`, if armed.
    gate: std::sync::Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[async_trait]
impl KvStorageInterface<RemovalRecord> for ParkingRemovals {
    async fn get(&self, key: &str) -> Result<Option<RemovalRecord>> {
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &RemovalRecord) -> Result<()> {
        let gate = self.gate.lock().map_err(|_| Error::LockPoisoned)?.take();
        if let Some((parked, release)) = gate {
            assert!(parked.send(()).is_ok(), "the test awaits the parked write");
            assert!(release.await.is_ok(), "the test releases the parked write");
        }
        self.inner.put(key, value).await
    }

    async fn get_all(&self) -> Result<Vec<(String, RemovalRecord)>> {
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

/// The lock law: a read at one key completes while a remove-side write at another key is parked
/// mid-flush, since a read takes no lock once the store is recovered.
#[tokio::test]
async fn test_a_read_proceeds_while_a_remove_side_write_is_parked() -> Result<()> {
    let removals = Arc::new(ParkingRemovals {
        inner: MemStorage::new(),
        gate: std::sync::Mutex::new(None),
    });
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    node.removal_storage = Box::new(removals.clone());
    let now_ms = get_epoch_ms();
    let writer = Did::from(7u32);
    let read_topic = Did::from(11u32);
    let added = stamped_delta(EntryOperation::Extend, read_topic, &["b"], now_ms, writer)?;
    node.join_storage_entry(now_ms, read_topic, added, writer)
        .await?;

    let (parked, parked_signal) = oneshot::channel();
    let (release_signal, release) = oneshot::channel();
    *removals.gate.lock().map_err(|_| Error::LockPoisoned)? = Some((parked, release));
    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    let removed = removes_only_carrier(Did::from(10u32), 1, now_ms, "parked")?;
    let write_key = Did::from(10u32).to_string();
    let write = store.put(&write_key, &removed, RemovalCharge::Holder, now_ms);
    let read = async {
        assert!(parked_signal.await.is_ok(), "the remove-side write parks");
        let read = store.get(&read_topic.to_string()).await;
        assert!(release_signal.send(()).is_ok(), "the write is still parked");
        read
    };
    let (written, read) = with_hang_guard(
        "a read beside a parked remove-side write",
        Duration::from_secs(5),
        async { futures::join!(write, read) },
    )
    .await;
    written?;
    assert_eq!(read?.map(|entry| entry.data.len()), Some(1));
    Ok(())
}
