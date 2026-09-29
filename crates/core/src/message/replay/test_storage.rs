//! The one storage double of the replay tests: a memory store decorated with hooks that run
//! before its operations, so each double states only the behaviour it changes.
//!
//! ```text
//! Hooked<H> = H ∘ MemStorage     get k ↦ H.before_get k; put k ↦ H.before_put k;
//!                                get_all, scan ↦ H.before_scan; remove k ↦ H.before_remove k
//! ```
//!
//! A hook that fails fails the operation before the store is touched; every other operation,
//! and every operation of the default hooks, is the memory store's own.

use crate::error::Result;
use crate::message::replay::ReplayRecord;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;
use crate::storage::MemStorage;
use crate::storage::ScannedRecord;

/// The behaviour a test double adds in front of its memory store; each hook passes by default.
#[async_trait::async_trait]
pub(super) trait StorageHooks: Send + Sync {
    /// Runs before every `get` of `key`; an error fails the read.
    async fn before_get(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    /// Runs before every `put` of `key`; an error fails the write.
    async fn before_put(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    /// Runs before every whole-store read (`get_all` and `scan`); an error fails the read.
    async fn before_scan(&self) -> Result<()> {
        Ok(())
    }

    /// Runs before every `remove` of `key`; an error fails the removal.
    async fn before_remove(&self, _key: &str) -> Result<()> {
        Ok(())
    }
}

/// A memory store behind `hooks` (see the module documentation).
pub(super) struct Hooked<H> {
    /// The stored records, reachable directly by the test.
    pub(super) inner: MemStorage<ReplayRecord>,
    /// The behaviour in front of the store.
    pub(super) hooks: H,
}

impl<H> Hooked<H> {
    /// An empty store behind `hooks`.
    pub(super) fn new(hooks: H) -> Self {
        Self {
            inner: MemStorage::new(),
            hooks,
        }
    }
}

#[async_trait::async_trait]
impl<H: StorageHooks> KvStorageInterface<ReplayRecord> for Hooked<H> {
    /// The hook, then the memory store's `get`.
    async fn get(&self, key: &str) -> Result<Option<ReplayRecord>> {
        self.hooks.before_get(key).await?;
        self.inner.get(key).await
    }

    /// The hook, then the memory store's `put`.
    async fn put(&self, key: &str, value: &ReplayRecord) -> Result<()> {
        self.hooks.before_put(key).await?;
        self.inner.put(key, value).await
    }

    /// The scan hook, then the memory store's `get_all`.
    async fn get_all(&self) -> Result<Vec<(String, ReplayRecord)>> {
        self.hooks.before_scan().await?;
        self.inner.get_all().await
    }

    /// The hook, then the memory store's `remove`.
    async fn remove(&self, key: &str) -> Result<()> {
        self.hooks.before_remove(key).await?;
        self.inner.remove(key).await
    }

    /// The memory store's `clear`.
    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }

    /// The memory store's `count`.
    async fn count(&self) -> Result<u32> {
        self.inner.count().await
    }
}

#[async_trait::async_trait]
impl<H: StorageHooks> KvStorageScan<ReplayRecord> for Hooked<H> {
    /// The scan hook, then the memory store's `scan`.
    async fn scan(&self) -> Result<Vec<ScannedRecord<ReplayRecord>>> {
        self.hooks.before_scan().await?;
        self.inner.scan().await
    }

    /// The memory store's naming: the key itself.
    fn record_name(&self, key: &str) -> String {
        self.inner.record_name(key)
    }
}
