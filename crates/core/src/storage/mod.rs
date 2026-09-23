//! Key-value storage adapters and their shared behavioral contract.
//!
//! Capacity is backend-specific and always counts the unit named by its constructor:
//! `MemStorage::bounded` and `IdbStorage` count keys/rows, while `FileStorage` counts
//! serialized record bytes. Bounded adapters preserve their
//! documented eviction order; memory and file use write recency, while IndexedDB uses access
//! recency. Replacing one adapter with another can therefore change both budget units and
//! retention order.

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
/// Persistent storage for native runtimes.
pub mod file;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// IndexedDB-backed storage for browser runtimes.
pub mod idb;
/// In-memory key value storage.
pub mod memory;
mod write_ordered;

use async_trait::async_trait;

use crate::error::Result;
pub use crate::storage::memory::MemStorage;

/// Backend-neutral operations over stored key-value pairs.
///
/// This interface does not imply a common capacity unit, eviction order, or overwrite effect.
/// Each bounded implementation documents whether its limit counts rows, keys, or serialized
/// bytes, how it chooses entries to retire, and how replacing a value affects the budget.
/// Backend-specific failure guarantees are documented by each implementation.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait KvStorageInterface<V> {
    /// Get a cache entry by `key`.
    async fn get(&self, key: &str) -> Result<Option<V>>;

    /// Put `entry` in the cache under `key`.
    async fn put(&self, key: &str, value: &V) -> Result<()>;

    /// Return every key value pair in this storage.
    async fn get_all(&self) -> Result<Vec<(String, V)>>;

    /// Remove an `entry` by `key`.
    async fn remove(&self, key: &str) -> Result<()>;

    /// Delete all values.
    async fn clear(&self) -> Result<()>;

    /// Get the current storage usage.
    async fn count(&self) -> Result<u32>;
}
