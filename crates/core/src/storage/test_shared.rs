//! Shared storages for tests: an `Arc` of a storage is that storage, so a test keeps a handle on
//! a storage it hands to a runtime, and restarts the runtime over it.

use std::sync::Arc;

use async_trait::async_trait;
use rings_runtime::MaybeSendSync;

use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;
use crate::storage::ScannedRecord;

/// A shared storage is the storage it shares: every operation delegates to it, so a wrapper
/// never restates (or mistakes) the naming its scan must agree with.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<V, S> KvStorageInterface<V> for Arc<S>
where
    V: MaybeSendSync,
    S: KvStorageInterface<V> + MaybeSendSync + ?Sized,
{
    /// `get` of the shared storage.
    async fn get(&self, key: &str) -> Result<Option<V>> {
        self.as_ref().get(key).await
    }

    /// `put` of the shared storage.
    async fn put(&self, key: &str, value: &V) -> Result<()> {
        self.as_ref().put(key, value).await
    }

    /// `get_all` of the shared storage.
    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        self.as_ref().get_all().await
    }

    /// `remove` of the shared storage.
    async fn remove(&self, key: &str) -> Result<()> {
        self.as_ref().remove(key).await
    }

    /// `clear` of the shared storage.
    async fn clear(&self) -> Result<()> {
        self.as_ref().clear().await
    }

    /// `count` of the shared storage.
    async fn count(&self) -> Result<u32> {
        self.as_ref().count().await
    }
}

/// A shared scannable storage scans and names exactly as the storage it shares.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<V, S> KvStorageScan<V> for Arc<S>
where
    V: MaybeSendSync,
    S: KvStorageScan<V> + MaybeSendSync + ?Sized,
{
    /// `scan` of the shared storage.
    async fn scan(&self) -> Result<Vec<ScannedRecord<V>>> {
        self.as_ref().scan().await
    }

    /// `record_name` of the shared storage.
    fn record_name(&self, key: &str) -> String {
        self.as_ref().record_name(key)
    }
}
