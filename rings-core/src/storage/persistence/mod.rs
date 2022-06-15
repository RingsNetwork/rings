#[cfg(feature = "wasm")]
pub mod idb;
#[cfg(feature = "default")]
pub mod kv;
use async_trait::async_trait;

#[cfg(feature = "wasm")]
pub use self::idb::IDBStorage;
use crate::err::Result;

/// Persistence Storage read and write functions
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(feature = "default", async_trait)]
pub trait PersistenceStorageReadAndWrite<K, V>: PersistenceStorageOperation {
    /// Get a cache entry by `key`.
    async fn get(&self, key: &K) -> Result<V>;

    /// Put `entry` in the cache under `key`.
    async fn put(&self, key: &K, entry: &V) -> Result<()>;

    async fn get_all(&self) -> Result<Vec<(K, V)>>;
}

/// Persistence Storage remove functions
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(feature = "default", async_trait)]
pub trait PersistenceStorageRemove<K>: PersistenceStorageOperation {
    /// Remove an `entry` by `key`.
    async fn remove(&self, key: &K) -> Result<()>;
}

/// Persistence Storage Operations
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(feature = "default", async_trait)]
pub trait PersistenceStorageOperation {
    /// Clear Storage.
    /// All `Entry` will be deleted.
    async fn clear(&self) -> Result<()>;

    /// Get the current storage usage, if applicable.
    async fn count(&self) -> Result<u64>;

    /// Get the maximum storage size, if applicable.
    async fn max_size(&self) -> Result<usize>;

    /// Get the storage size, if applicable.
    async fn total_size(&self) -> Result<usize>;

    /// Prune database storage
    async fn prune(&self) -> Result<()>;
}

pub mod redis;
