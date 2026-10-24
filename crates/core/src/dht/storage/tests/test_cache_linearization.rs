//! The fetch cache's linearization (#922; removal-stability paper, Law 7 and Prop. 8): a cache
//! read is pure, so a read that loaded a dead value before a concurrent put completed cannot
//! drop that put, and every state change is a transition under the cache transition.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::channel::oneshot;

use super::super::super::chord::PeerRing;
use super::data_entry;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::tests::expired;
use crate::tests::stamped_delta;
use crate::utils::get_epoch_ms;

/// The two ends of a parked `get`: the store reports that it parked, then waits for release.
struct Gate {
    /// Fired once the gated `get` has read its value and parked.
    parked: oneshot::Sender<()>,
    /// Awaited by the parked `get` before it returns the value it read.
    release: oneshot::Receiver<()>,
}

/// A cache store that counts its writes and can park the next `get` after it read its value,
/// so a test can place a transition between a read's load and its return.
///
/// Law: the counter rises by one for every `put`, `remove` and `clear`, and by nothing else.
struct GatedStore {
    /// The store the operations reach.
    inner: MemStorage<Entry>,
    /// Writes (`put`, `remove`, `clear`) issued so far.
    writes: AtomicUsize,
    /// The gate the next `get` parks on, if armed.
    gate: Mutex<Option<Gate>>,
}

impl GatedStore {
    /// An empty, unarmed store.
    fn new() -> Self {
        Self {
            inner: MemStorage::new(),
            writes: AtomicUsize::new(0),
            gate: Mutex::new(None),
        }
    }

    /// Park the next `get` after its read: the returned receiver fires once it has parked, and
    /// the returned sender releases it.
    fn arm(&self) -> Result<(oneshot::Receiver<()>, oneshot::Sender<()>)> {
        let (parked, parked_signal) = oneshot::channel();
        let (release_signal, release) = oneshot::channel();
        *self.gate.lock().map_err(|_| Error::LockPoisoned)? = Some(Gate { parked, release });
        Ok((parked_signal, release_signal))
    }

    /// Record one write.
    fn count_write(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl KvStorageInterface<Entry> for GatedStore {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        let value = self.inner.get(key).await?;
        let gate = self.gate.lock().map_err(|_| Error::LockPoisoned)?.take();
        if let Some(gate) = gate {
            assert!(
                gate.parked.send(()).is_ok(),
                "the test awaits the parked signal"
            );
            assert!(
                gate.release.await.is_ok(),
                "the test releases the parked read"
            );
        }
        Ok(value)
    }

    async fn put(&self, key: &str, value: &Entry) -> Result<()> {
        self.count_write();
        self.inner.put(key, value).await
    }

    async fn get_all(&self) -> Result<Vec<(String, Entry)>> {
        self.inner.get_all().await
    }

    async fn remove(&self, key: &str) -> Result<()> {
        self.count_write();
        self.inner.remove(key).await
    }

    async fn clear(&self) -> Result<()> {
        self.count_write();
        self.inner.clear().await
    }

    async fn count(&self) -> Result<u32> {
        self.inner.count().await
    }
}

/// The trace of #922 cannot occur: a read loads a dead cached value `V0` and parks, a put of
/// `V1` runs to completion meanwhile (it does not wait on the read, which holds no transition),
/// and the read then returns without a write, so the cache still holds `V1`.
#[tokio::test]
async fn test_cache_read_does_not_drop_a_concurrent_put() -> Result<()> {
    let resource = Did::from(10u32);
    let store = Arc::new(GatedStore::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    node.cache = Box::new(store.clone());
    store
        .inner
        .put(&resource.to_string(), &expired(data_entry(resource)))
        .await?;
    let fresh = stamped_delta(
        EntryOperation::Extend,
        resource,
        &["v1"],
        get_epoch_ms(),
        Did::from(1u32),
    )?;

    let (parked, release) = store.arm()?;
    let reader = node.local_cache_get(resource);
    let writer = async {
        assert!(parked.await.is_ok(), "the read parks after its load");
        node.local_cache_put(fresh.clone()).await?;
        let writes_with_put = store.writes.load(Ordering::SeqCst);
        assert!(release.send(()).is_ok(), "the parked read is still waiting");
        Ok::<_, Error>(writes_with_put)
    };
    let (read, writes_with_put) = futures::join!(reader, writer);

    assert_eq!(
        read?, None,
        "the read served the dead value it loaded as absent"
    );
    assert_eq!(
        store.writes.load(Ordering::SeqCst),
        writes_with_put?,
        "the read wrote nothing after it loaded"
    );
    let cached = node.local_cache_get(resource).await?;
    assert_eq!(cached.map(|entry| entry.data), Some(fresh.data));
    Ok(())
}

/// A read writes nothing, whatever it projects: a dead value is reported absent and kept, and
/// a live one is returned without a write-back.
#[tokio::test]
async fn test_cache_read_is_pure() -> Result<()> {
    let dead = Did::from(10u32);
    let live = Did::from(11u32);
    let store = Arc::new(GatedStore::new());
    let mut node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    node.cache = Box::new(store.clone());
    store
        .inner
        .put(&dead.to_string(), &expired(data_entry(dead)))
        .await?;
    node.local_cache_put(stamped_delta(
        EntryOperation::Extend,
        live,
        &["a"],
        get_epoch_ms(),
        Did::from(1u32),
    )?)
    .await?;
    let writes_before = store.writes.load(Ordering::SeqCst);

    assert_eq!(node.local_cache_get(dead).await?, None);
    assert!(node.local_cache_get(live).await?.is_some());
    assert!(node.local_cache_held(dead, get_epoch_ms()).await?.is_none());

    assert_eq!(store.writes.load(Ordering::SeqCst), writes_before);
    assert_eq!(store.inner.count().await?, 2);
    Ok(())
}
