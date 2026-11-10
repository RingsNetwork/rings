//! The factored data store (#920; removal-stability paper, Laws 2–4): eviction drops element
//! sides and never a remove, the removal store is shared by slot (#907; load-bounds paper,
//! Sec. V), and the recovery pass normalizes an element side a crash rolled back.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::oneshot;
use rings_test_support::with_hang_guard;

use super::super::super::chord::PeerRing;
use crate::consts::DATA_TOMBSTONE_MAX_LEN;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::ChargedRemove;
use crate::dht::Did;
use crate::dht::RemovalRecord;
use crate::dht::StorageKey;
use crate::dht::REMOVAL_SHARES;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::SlotFloorUse;
use crate::message::Principal;
use crate::message::ShareRefusal;
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
    node.join_storage_entry(now_ms, topic, removed, Principal::Unbound)
        .await?;

    let filler = stamped_delta(
        EntryOperation::Extend,
        Did::from(11u32),
        &["b"],
        now_ms,
        writer,
    )?;
    node.join_storage_entry(now_ms, Did::from(11u32), filler, Principal::Unbound)
        .await?;
    assert_eq!(node.storage.get(&topic.to_string()).await?, None, "evicted");

    node.join_storage_entry(now_ms, topic, added, Principal::Unbound)
        .await?;
    let read = stored(&node, topic, now_ms).await?;
    assert_eq!(read.as_ref().map(|entry| entry.data.len()), Some(0));
    assert_eq!(read.map(|entry| entry.crdt.tombstones.len()), Some(1));
    Ok(())
}

/// Fill `slot`'s floor and as much of the pool as one claimant may take: `f + P` removes, in
/// carriers of at most `DATA_TOMBSTONE_MAX_LEN`, on topics from `first_topic` on.
async fn fill_floor_and_pool(
    node: &PeerRing,
    slot: Principal,
    first_topic: u32,
    now_ms: u128,
) -> Result<()> {
    let mut left = REMOVAL_SHARES.floor + REMOVAL_SHARES.pool;
    let mut topic = first_topic;
    while left > 0 {
        let removes = left.min(DATA_TOMBSTONE_MAX_LEN);
        let carrier = removes_only_carrier(Did::from(topic), removes, now_ms, "fill")?;
        node.join_storage_entry(now_ms, Did::from(topic), carrier, slot)
            .await?;
        left -= removes;
        topic += 1;
    }
    Ok(())
}

/// Isolation (load-bounds paper, Theorem 1): a slot that holds its floor and the whole pool, as
/// a lone claimant may, is refused a further remove, which changes nothing, and so is a delivery
/// on no slot; another slot below its floor and this node still remove, and each remove is
/// charged to the slot of the edge that delivered it.
#[tokio::test]
async fn test_a_slot_past_its_claim_is_refused_and_another_floor_is_not() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let (flooder, honest) = (Did::from(70u32), Did::from(71u32));
    node.edges().arrive(flooder, &SlotFloorUse::default());
    node.edges().arrive(honest, &SlotFloorUse::default());
    let flooder = node.edges().principal_of(Some(flooder));
    let honest = node.edges().principal_of(Some(honest));
    assert_eq!((flooder, honest), (Principal::Slot(0), Principal::Slot(1)));
    fill_floor_and_pool(&node, flooder, 1_000, now_ms).await?;

    let topic = Did::from(11u32);
    let one_more = removes_only_carrier(topic, 1, now_ms, "more")?;
    for refused in [flooder, Principal::Unbound] {
        assert!(matches!(
            node.join_storage_entry(now_ms, topic, one_more.clone(), refused)
                .await,
            Err(Error::RemovalCapacityExhausted {
                refusal: ShareRefusal::PoolFull { .. },
                ..
            })
        ));
    }
    assert_eq!(stored(&node, topic, now_ms).await?, None);

    node.join_storage_entry(now_ms, topic, one_more, honest)
        .await?;
    let local_topic = Did::from(12u32);
    let local = removes_only_carrier(local_topic, 1, now_ms, "local")?;
    node.join_storage_entry(now_ms, local_topic, local, Principal::Local)
        .await?;
    let charges = node
        .removal_storage
        .get(&topic.to_string())
        .await?
        .map(|record| {
            record
                .removes
                .iter()
                .map(|remove| remove.charge)
                .collect::<Vec<_>>()
        });
    assert_eq!(charges, Some(vec![honest]));
    Ok(())
}

/// Rotation residual (Prop. 1): a slot's removes outlive the edge that delivered them, so an edge
/// later given that slot inherits them; once they are stable, the slot removes again.
#[tokio::test]
async fn test_a_slot_keeps_its_removes_until_they_are_stable() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let (rotated, fresh) = (Did::from(70u32), Did::from(72u32));
    node.edges().arrive(rotated, &SlotFloorUse::default());
    let slot = node.edges().principal_of(Some(rotated));
    fill_floor_and_pool(&node, slot, 1_000, now_ms).await?;
    node.edges().depart(rotated);
    // Every slot read as clean, so the fresh edge takes the least index, the rotated slot: the
    // subject here is the inheritance, not the choice of slot (`slot_roster`, cleanest first).
    node.edges().arrive(fresh, &SlotFloorUse::default());
    assert_eq!(node.edges().principal_of(Some(fresh)), slot);

    let topic = Did::from(9u32);
    let late = removes_only_carrier(topic, 1, now_ms, "inherited")?;
    assert!(node
        .join_storage_entry(now_ms, topic, late, slot)
        .await
        .is_err());

    let horizon_ms = u128::from(EntryKind::Data.max_lifetime_ms()) + TS_OFFSET_TOLERANCE_MS;
    let stable_ms = now_ms + horizon_ms;
    let late = removes_only_carrier(topic, 1, stable_ms, "late")?;
    node.join_storage_entry(stable_ms, topic, late, slot)
        .await?;
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
                charge: Principal::Unbound,
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
    node.removal_storage = Arc::new(Box::new(removals.clone()));
    let now_ms = get_epoch_ms();
    let writer = Did::from(7u32);
    let read_topic = Did::from(11u32);
    let added = stamped_delta(EntryOperation::Extend, read_topic, &["b"], now_ms, writer)?;
    node.join_storage_entry(now_ms, read_topic, added, Principal::Unbound)
        .await?;

    let (parked, parked_signal) = oneshot::channel();
    let (release_signal, release) = oneshot::channel();
    *removals.gate.lock().map_err(|_| Error::LockPoisoned)? = Some((parked, release));
    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    let removed = removes_only_carrier(Did::from(10u32), 1, now_ms, "parked")?;
    let write_key = Did::from(10u32).to_string();
    let write = store.put(&write_key, &removed, Principal::Local, now_ms);
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

/// Construct a synthetic storage I/O error for fault injection.
fn injected_io_error() -> Error {
    Error::ServiceIOError(std::io::Error::other("injected storage error"))
}

/// Fault-injection state for [`FaultStorage`].
#[derive(Default)]
struct FaultState {
    /// `(target_key, skip_matches)` whose `get` returns an injected I/O error once `skip_matches`
    /// matching calls have passed.
    fail_get_key: Option<(String, usize)>,
    /// Number of upcoming `put` calls that return an injected I/O error without committing.
    fail_put_count: usize,
    /// If true, the next `put` writes to `inner` first and then returns an injected I/O error.
    commit_then_err_put: bool,
    /// If true, the next `remove` deletes from `inner` first and then returns an injected I/O
    /// error.
    commit_then_err_remove: bool,
    /// Armed gate for `get(target_key)`: signals when reached and waits for release.
    park_get_key: Option<(String, oneshot::Sender<()>, oneshot::Receiver<()>)>,
    /// Armed gate for `put`: `(commit_before_park, parked_tx, release_rx)`.
    park_put: Option<(bool, oneshot::Sender<()>, oneshot::Receiver<()>)>,
    /// Armed gate for `remove`: `(commit_before_park, parked_tx, release_rx)`.
    park_remove: Option<(bool, oneshot::Sender<()>, oneshot::Receiver<()>)>,
}

/// In-memory storage wrapper supporting deterministic fault injection and mid-await parking/drop.
struct FaultStorage<V> {
    /// Underlying in-memory store.
    inner: MemStorage<V>,
    /// Armed fault configuration.
    state: std::sync::Mutex<FaultState>,
}

impl<V> FaultStorage<V>
where V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static
{
    /// Construct an empty fault-injectable storage.
    fn new() -> Self {
        Self {
            inner: MemStorage::new(),
            state: std::sync::Mutex::new(FaultState::default()),
        }
    }

    /// Lock fault state for configuration in tests.
    fn configure(&self, update: impl FnOnce(&mut FaultState)) -> Result<()> {
        let mut guard = self.state.lock().map_err(|_| Error::LockPoisoned)?;
        update(&mut guard);
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<V> KvStorageInterface<V> for FaultStorage<V>
where V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        let parked = {
            let mut state = self.state.lock().map_err(|_| Error::LockPoisoned)?;
            if let Some((target, skip)) = state.fail_get_key.as_mut() {
                if target == key {
                    if *skip == 0 {
                        state.fail_get_key = None;
                        return Err(injected_io_error());
                    }
                    *skip -= 1;
                }
            }
            match state.park_get_key.as_ref() {
                Some((target, _, _)) if target == key => {
                    state.park_get_key.take().map(|(_, tx, rx)| (tx, rx))
                }
                _ => None,
            }
        };
        if let Some((tx, rx)) = parked {
            let _ = tx.send(());
            let _ = rx.await;
        }
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        enum Action {
            FailUncommitted,
            CommitThenErr,
            Park {
                commit_before_park: bool,
                tx: oneshot::Sender<()>,
                rx: oneshot::Receiver<()>,
            },
            Normal,
        }
        let action = {
            let mut state = self.state.lock().map_err(|_| Error::LockPoisoned)?;
            if let Some((commit_before_park, tx, rx)) = state.park_put.take() {
                Action::Park {
                    commit_before_park,
                    tx,
                    rx,
                }
            } else if state.commit_then_err_put {
                state.commit_then_err_put = false;
                Action::CommitThenErr
            } else if state.fail_put_count > 0 {
                state.fail_put_count -= 1;
                Action::FailUncommitted
            } else {
                Action::Normal
            }
        };
        match action {
            Action::FailUncommitted => Err(injected_io_error()),
            Action::CommitThenErr => {
                self.inner.put(key, value).await?;
                Err(injected_io_error())
            }
            Action::Park {
                commit_before_park,
                tx,
                rx,
            } => {
                if commit_before_park {
                    self.inner.put(key, value).await?;
                }
                let _ = tx.send(());
                let released = rx.await.is_ok();
                if !commit_before_park && released {
                    self.inner.put(key, value).await?;
                }
                Ok(())
            }
            Action::Normal => self.inner.put(key, value).await,
        }
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        self.inner.get_all().await
    }

    async fn remove(&self, key: &str) -> Result<()> {
        enum Action {
            CommitThenErr,
            Park {
                commit_before_park: bool,
                tx: oneshot::Sender<()>,
                rx: oneshot::Receiver<()>,
            },
            Normal,
        }
        let action = {
            let mut state = self.state.lock().map_err(|_| Error::LockPoisoned)?;
            if let Some((commit_before_park, tx, rx)) = state.park_remove.take() {
                Action::Park {
                    commit_before_park,
                    tx,
                    rx,
                }
            } else if state.commit_then_err_remove {
                state.commit_then_err_remove = false;
                Action::CommitThenErr
            } else {
                Action::Normal
            }
        };
        match action {
            Action::CommitThenErr => {
                self.inner.remove(key).await?;
                Err(injected_io_error())
            }
            Action::Park {
                commit_before_park,
                tx,
                rx,
            } => {
                if commit_before_park {
                    self.inner.remove(key).await?;
                }
                let _ = tx.send(());
                let released = rx.await.is_ok();
                if !commit_before_park && released {
                    self.inner.remove(key).await?;
                }
                Ok(())
            }
            Action::Normal => self.inner.remove(key).await,
        }
    }

    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }

    async fn count(&self) -> Result<u32> {
        self.inner.count().await
    }
}

/// Build a `RemovalRecord` for `topic` with `count` `charge`-attributed removes at `now_ms`.
fn seeded_removal_record(
    topic: Did,
    count: usize,
    now_ms: u128,
    tag: &str,
    charge: Principal,
) -> Result<RemovalRecord> {
    let carrier = removes_only_carrier(topic, count, now_ms, tag)?;
    Ok(RemovalRecord {
        did: topic,
        register: None,
        removes: carrier
            .crdt
            .tombstones
            .into_iter()
            .map(|tombstone| ChargedRemove { tombstone, charge })
            .collect(),
    })
}

/// Regression (#1203, #1205 item 3): mid-scan `elements.get` or `elements.put` failures during
/// `CarrierStore::ensure_recovered` leave zero partial charges in `removal_index`, so a retry
/// charges each persisted removal once and preserves the remaining `Local` share.
#[tokio::test]
async fn test_recovery_retry_after_mid_scan_failure_charges_each_removal_once() -> Result<()> {
    let elements = Arc::new(FaultStorage::<Entry>::new());
    let removals = Arc::new(FaultStorage::<RemovalRecord>::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(elements.clone()));
    node.removal_storage = Arc::new(Box::new(removals.clone()));

    let now_ms = get_epoch_ms();
    let writer = Did::from(7u32);
    let mut last_key = String::new();
    let mut last_topic = Did::from(0u32);

    // Seed 8 records of 512 Local removals each = 4,096 total (< Local share of 7,168).
    for idx in 0..8u32 {
        let topic = Did::from(100 + idx);
        let key = topic.to_string();
        if idx == 7 {
            let added = stamped_delta(EntryOperation::Extend, topic, &["stale"], now_ms, writer)?
                .try_into_storage_entry()?;
            let removed = added.tombstone(added.clone())?;
            let mut record = seeded_removal_record(topic, 511, now_ms, "batch", Principal::Local)?;
            record
                .removes
                .extend(
                    removed
                        .crdt
                        .tombstones
                        .into_iter()
                        .map(|tombstone| ChargedRemove {
                            tombstone,
                            charge: Principal::Local,
                        }),
                );
            assert_eq!(record.removes.len(), 512);
            elements.inner.put(&key, &added).await?;
            removals.inner.put(&key, &record).await?;
            last_key = key;
            last_topic = topic;
        } else {
            let record = seeded_removal_record(topic, 512, now_ms, "batch", Principal::Local)?;
            removals.inner.put(&key, &record).await?;
        }
    }

    // 1st recovery attempt: fail `elements.get` on the final record.
    elements.configure(|state| state.fail_get_key = Some((last_key.clone(), 0)))?;
    assert!(stored(&node, last_topic, now_ms).await.is_err());
    assert_eq!(
        node.removal_held(Principal::Local),
        0,
        "failed recovery must not publish partial charges"
    );

    // 2nd recovery attempt: fail `elements.put` when normalizing the final record.
    elements.configure(|state| state.fail_put_count = 1)?;
    assert!(stored(&node, last_topic, now_ms).await.is_err());
    assert_eq!(
        node.removal_held(Principal::Local),
        0,
        "failed normalization write must not publish partial charges"
    );

    // 3rd recovery attempt: succeeds cleanly and charges each persisted removal once (4,096).
    let recovered_entry = stored(&node, last_topic, now_ms).await?;
    assert_eq!(
        recovered_entry.map(|entry| entry.data),
        Some(Vec::<Bytes>::new())
    );
    assert_eq!(node.removal_held(Principal::Local), 4_096);

    // Remaining Local capacity is 7,168 - 4,096 = 3,072; fill all 3,072 without refusal.
    for extra in 0..6u32 {
        let extra_topic = Did::from(500 + extra);
        let extra_carrier = removes_only_carrier(extra_topic, 512, now_ms, "extra")?;
        node.join_storage_entry(now_ms, extra_topic, extra_carrier, Principal::Local)
            .await?;
    }
    assert_eq!(node.removal_held(Principal::Local), REMOVAL_SHARES.local);
    Ok(())
}

/// Regression (#1203, #1205 item 3): cancelling `ensure_recovered` mid-scan does not leak partial
/// charges into `removal_index`, and a subsequent retry charges each record once.
#[tokio::test]
async fn test_recovery_retry_after_mid_scan_cancellation_charges_each_removal_once() -> Result<()> {
    let elements = Arc::new(FaultStorage::<Entry>::new());
    let removals = Arc::new(FaultStorage::<RemovalRecord>::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(elements.clone()));
    node.removal_storage = Arc::new(Box::new(removals.clone()));

    let now_ms = get_epoch_ms();
    for idx in 0..4u32 {
        let topic = Did::from(200 + idx);
        let record = seeded_removal_record(topic, 512, now_ms, "cancel", Principal::Local)?;
        removals.inner.put(&topic.to_string(), &record).await?;
    }
    let final_key = Did::from(203u32).to_string();
    let (parked_tx, parked_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    elements.configure(|state| {
        state.park_get_key = Some((final_key.clone(), parked_tx, release_rx));
    })?;

    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    with_hang_guard("cancel mid-scan recovery", Duration::from_secs(5), async {
        let mut fut = Box::pin(store.get(&final_key));
        tokio::select! {
            _ = &mut fut => panic!("recovery should park on final key"),
            res = parked_rx => assert!(res.is_ok(), "parked on final key"),
        }
        drop(fut);
    })
    .await;

    assert_eq!(
        node.removal_held(Principal::Local),
        0,
        "cancelled recovery must not leave partial charges"
    );

    let read = store.get(&final_key).await?;
    assert_eq!(read.map(|entry| entry.crdt.tombstones.len()), Some(512));
    assert_eq!(node.removal_held(Principal::Local), 2_048);
    Ok(())
}

/// Regression (#1205 item 4): when `removals.put` or `removals.remove` returns `Err` after
/// committing in the backend (e.g. directory-sync failure), `CarrierStore` reconciles accounting
/// against actual stored records instead of blindly reverting or leaking charges.
#[tokio::test]
async fn test_uncertain_removal_commits_reconcile_accounting_on_error() -> Result<()> {
    let elements = Arc::new(FaultStorage::<Entry>::new());
    let removals = Arc::new(FaultStorage::<RemovalRecord>::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(elements.clone()));
    node.removal_storage = Arc::new(Box::new(removals.clone()));

    let now_ms = get_epoch_ms();
    let writer = Did::from(7u32);
    let topic = Did::from(300u32);
    let key = topic.to_string();
    let added = stamped_delta(EntryOperation::Extend, topic, &["v1"], now_ms, writer)?
        .try_into_storage_entry()?;
    let removed = added.tombstone(added.clone())?;

    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    store.put(&key, &added, Principal::Local, now_ms).await?;
    assert_eq!(node.removal_held(Principal::Local), 0);

    // 1. Uncommitted `removals.put` failure reverts the charge.
    removals.configure(|state| state.fail_put_count = 1)?;
    assert!(store
        .put(&key, &removed, Principal::Local, now_ms)
        .await
        .is_err());
    assert_eq!(node.removal_held(Principal::Local), 0);

    // 2. Committed-but-error `removals.put` keeps the charge AND marks elements dirty so reads
    //    do not resurrect "v1", and a retry seeing `next == previous` does not double-charge.
    removals.configure(|state| state.commit_then_err_put = true)?;
    assert!(store
        .put(&key, &removed, Principal::Local, now_ms)
        .await
        .is_err());
    assert_eq!(
        node.removal_held(Principal::Local),
        1,
        "committed-but-error removal write must retain its accounting charge"
    );
    let read_after_partial = store.get(&key).await?;
    assert_eq!(
        read_after_partial.as_ref().map(|entry| entry.data.len()),
        Some(0),
        "covered element must not remain visible after committed-but-error removal write"
    );

    // Retry of the same `removed` value sees `next == previous` and finishes cleanly with charge 1.
    store.put(&key, &removed, Principal::Local, now_ms).await?;
    assert_eq!(node.removal_held(Principal::Local), 1);

    // 3. Committed-but-error `removals.remove` discharges the removed record's charge.
    removals.configure(|state| state.commit_then_err_remove = true)?;
    assert!(store.remove(&key).await.is_err());
    assert_eq!(
        node.removal_held(Principal::Local),
        0,
        "committed-but-error removal delete must discharge its accounting charge"
    );
    store.remove(&key).await?;
    assert_eq!(node.removal_held(Principal::Local), 0);

    // 4. Committed-but-error `removals.put` where the immediate read-back `removals.get` ALSO
    //    fails marks `recovered = false` so the next operation rebuilds accounting and normalizes.
    store.put(&key, &added, Principal::Local, now_ms).await?;
    removals.configure(|state| {
        state.commit_then_err_put = true;
        state.fail_get_key = Some((key.clone(), 1));
    })?;
    assert!(store
        .put(&key, &removed, Principal::Local, now_ms)
        .await
        .is_err());
    let recovered_after_outage = store.get(&key).await?;
    assert_eq!(
        recovered_after_outage
            .as_ref()
            .map(|entry| entry.data.len()),
        Some(0)
    );
    assert_eq!(node.removal_held(Principal::Local), 1);
    Ok(())
}

/// Regression (#1205 item 4): cancelling `CarrierStore::put` or `CarrierStore::remove` while the
/// removal-store operation is in flight reconciles accounting on the next operation whether the
/// backend committed before cancellation or not.
#[tokio::test]
async fn test_cancelled_removal_writes_and_deletes_reconcile_on_next_operation() -> Result<()> {
    let elements = Arc::new(FaultStorage::<Entry>::new());
    let removals = Arc::new(FaultStorage::<RemovalRecord>::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(elements.clone()));
    node.removal_storage = Arc::new(Box::new(removals.clone()));

    let now_ms = get_epoch_ms();
    let topic = Did::from(310u32);
    let key = topic.to_string();
    let writer = Did::from(7u32);
    let added = stamped_delta(EntryOperation::Extend, topic, &["v1"], now_ms, writer)?
        .try_into_storage_entry()?;
    let removed = added.tombstone(added.clone())?;

    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    store.put(&key, &added, Principal::Local, now_ms).await?;

    // A. Cancel `put` before `removals` commits: charge is 0 on drop and 1 after retry.
    let (parked_tx, parked_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    removals.configure(|state| state.park_put = Some((false, parked_tx, release_rx)))?;
    with_hang_guard("cancel uncommitted put", Duration::from_secs(5), async {
        let mut fut = Box::pin(store.put(&key, &removed, Principal::Local, now_ms));
        tokio::select! {
            _ = &mut fut => panic!("put should park before commit"),
            res = parked_rx => assert!(res.is_ok()),
        }
        drop(fut);
    })
    .await;
    drop(release_tx);
    assert_eq!(node.removal_held(Principal::Local), 0);

    // B. Cancel `put` AFTER `removals` already committed (e.g. parked in directory sync):
    //    the next read/retry reconciles accounting to 1 and normalizes `E_old`.
    let (parked_tx, parked_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    removals.configure(|state| state.park_put = Some((true, parked_tx, release_rx)))?;
    with_hang_guard("cancel committed put", Duration::from_secs(5), async {
        let mut fut = Box::pin(store.put(&key, &removed, Principal::Local, now_ms));
        tokio::select! {
            _ = &mut fut => panic!("put should park after commit"),
            res = parked_rx => assert!(res.is_ok()),
        }
        drop(fut);
    })
    .await;
    drop(release_tx);

    let read = store.get(&key).await?;
    assert_eq!(read.as_ref().map(|entry| entry.data.len()), Some(0));
    assert_eq!(node.removal_held(Principal::Local), 1);

    // Retry `put` with the same value does not double-charge.
    store.put(&key, &removed, Principal::Local, now_ms).await?;
    assert_eq!(node.removal_held(Principal::Local), 1);

    // C. Cancel `remove` AFTER `removals` already deleted the record: next operation reconciles to 0.
    let (parked_tx, parked_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    removals.configure(|state| state.park_remove = Some((true, parked_tx, release_rx)))?;
    with_hang_guard("cancel committed remove", Duration::from_secs(5), async {
        let mut fut = Box::pin(store.remove(&key));
        tokio::select! {
            _ = &mut fut => panic!("remove should park after commit"),
            res = parked_rx => assert!(res.is_ok()),
        }
        drop(fut);
    })
    .await;
    drop(release_tx);

    assert_eq!(store.get(&key).await?, None);
    assert_eq!(node.removal_held(Principal::Local), 0);
    Ok(())
}

/// Regression (#1205 item 5): when `removals.put` succeeds and `elements.put` fails or is
/// cancelled, subsequent `get`, `get_all`, and `live_storage_entry` reads without restarting
/// normalize `E_old` against `R_new` and never serve tombstoned elements.
#[tokio::test]
async fn test_partial_carrier_write_normalizes_reads_without_restart() -> Result<()> {
    let elements = Arc::new(FaultStorage::<Entry>::new());
    let removals = Arc::new(FaultStorage::<RemovalRecord>::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(elements.clone()));
    node.removal_storage = Arc::new(Box::new(removals.clone()));

    let now_ms = get_epoch_ms();
    let writer = Did::from(7u32);

    // Case 1: `removals.put` succeeds, then `elements.put` fails with an error.
    let topic_a = Did::from(400u32);
    let key_a = topic_a.to_string();
    let added_a = stamped_delta(
        EntryOperation::Extend,
        topic_a,
        &["secret-a"],
        now_ms,
        writer,
    )?;
    node.join_storage_entry(now_ms, topic_a, added_a.clone(), Principal::Local)
        .await?;
    let removed_a = added_a.tombstone(added_a.clone())?;

    elements.configure(|state| state.fail_put_count = 1)?;
    assert!(node
        .join_storage_entry(now_ms, topic_a, removed_a.clone(), Principal::Local)
        .await
        .is_err());
    assert_eq!(
        elements
            .inner
            .get(&key_a)
            .await?
            .map(|entry| entry.data.len()),
        Some(1),
        "raw element store still holds E_old before retry"
    );

    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    let direct_get = store.get(&key_a).await?;
    assert_eq!(
        direct_get.as_ref().map(|entry| entry.data.len()),
        Some(0),
        "store.get must normalize E_old against R_new"
    );
    assert_eq!(direct_get.map(|entry| entry.crdt.tombstones.len()), Some(1));

    let all = store.get_all().await?;
    let from_all = all
        .into_iter()
        .find(|(k, _)| k == &key_a)
        .map(|(_, entry)| entry);
    assert_eq!(from_all.as_ref().map(|entry| entry.data.len()), Some(0));
    let live_a = stored(&node, topic_a, now_ms).await?;
    assert_eq!(live_a.map(|entry| entry.data.len()), Some(0));

    // Retry of the write repairs `elements` on disk and clears the dirty flag.
    node.join_storage_entry(now_ms, topic_a, removed_a, Principal::Local)
        .await?;
    assert_eq!(
        elements
            .inner
            .get(&key_a)
            .await?
            .map(|entry| entry.data.len()),
        Some(0),
        "retry repaired E_old on disk"
    );

    // Case 2: `removals.put` succeeds, then `elements.put` is cancelled mid-await.
    let topic_b = Did::from(401u32);
    let key_b = topic_b.to_string();
    let added_b = stamped_delta(
        EntryOperation::Extend,
        topic_b,
        &["secret-b"],
        now_ms,
        writer,
    )?
    .try_into_storage_entry()?;
    store
        .put(&key_b, &added_b, Principal::Local, now_ms)
        .await?;
    let removed_b = added_b.tombstone(added_b.clone())?;

    let (parked_tx, parked_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    elements.configure(|state| state.park_put = Some((false, parked_tx, release_rx)))?;
    with_hang_guard("cancel elements.put", Duration::from_secs(5), async {
        let mut fut = Box::pin(store.put(&key_b, &removed_b, Principal::Local, now_ms));
        tokio::select! {
            _ = &mut fut => panic!("put should park on elements.put"),
            res = parked_rx => assert!(res.is_ok()),
        }
        drop(fut);
    })
    .await;
    assert_eq!(
        elements
            .inner
            .get(&key_b)
            .await?
            .map(|entry| entry.data.len()),
        Some(1),
        "cancelled elements.put left E_old in raw element store"
    );

    let live_b = stored(&node, topic_b, now_ms).await?;
    assert_eq!(
        live_b.map(|entry| entry.data.len()),
        Some(0),
        "live read after cancelled elements.put must not expose tombstoned element"
    );
    store
        .put(&key_b, &removed_b, Principal::Local, now_ms)
        .await?;
    assert_eq!(
        elements
            .inner
            .get(&key_b)
            .await?
            .map(|entry| entry.data.len()),
        Some(0),
        "subsequent write repaired E_old on disk"
    );
    Ok(())
}

/// Regression: when a caller cancels `CarrierStore::put` while a detached removal-store write is
/// still in flight and that write commits after `ensure_recovered` begins, recovery waits for the
/// in-flight write to finish before scanning or publishing `recovered = true`.
#[tokio::test]
async fn test_recovery_waits_for_a_cancelled_backends_late_commit() -> Result<()> {
    let elements = Arc::new(FaultStorage::<Entry>::new());
    let removals = Arc::new(FaultStorage::<RemovalRecord>::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(elements.clone()));
    node.removal_storage = Arc::new(Box::new(removals.clone()));

    let now_ms = get_epoch_ms();
    let topic = Did::from(410u32);
    let key = topic.to_string();
    let writer = Did::from(7u32);
    let added = stamped_delta(EntryOperation::Extend, topic, &["v1"], now_ms, writer)?
        .try_into_storage_entry()?;
    let removed = added.tombstone(added.clone())?;

    let store = node
        .carrier_storage(EntryKind::Data)
        .ok_or(Error::RelayStorageUnavailable)?;
    store.put(&key, &added, Principal::Local, now_ms).await?;

    let (parked_tx, parked_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    removals.configure(|state| state.park_put = Some((false, parked_tx, release_rx)))?;

    with_hang_guard("cancel put before late backend commit", Duration::from_secs(5), async {
        let mut put_fut = Box::pin(store.put(&key, &removed, Principal::Local, now_ms));
        tokio::select! {
            _ = &mut put_fut => panic!("put should park inside removals.put"),
            res = parked_rx => assert!(res.is_ok()),
        }
        // Cancel the caller while the removal-store write is still parked.
        drop(put_fut);

        // Start a read (`get`), which triggers `ensure_recovered`, poll it once while the
        // cancelled write is still parked, then release the backend write to commit.
        let mut get_fut = Box::pin(store.get(&key));
        assert!(futures::poll!(&mut get_fut).is_pending());
        assert!(release_tx.send(()).is_ok());

        let read = get_fut.await?;
        assert_eq!(read.as_ref().map(|entry| entry.data.len()), Some(0));
        assert_eq!(node.removal_held(Principal::Local), 1);
        Ok::<(), Error>(())
    })
    .await?;
    Ok(())
}
