#![deny(missing_docs)]

//! IndexedDB adapter with atomic updates and row-budget LRU eviction.
//!
//! The configured capacity counts rows. When a new key would exceed it, the least recently
//! accessed rows are retired first; rewriting an existing key does not change row count or
//! retire another key. Opening an existing database with a smaller capacity retires every
//! excess row in the same transaction. Row metadata orders eviction by `last_visit_time`.
//! Opening an existing database neither upgrades its schema nor clears data within capacity.

use async_trait::async_trait;
use rexie::Index;
use rexie::ObjectStore;
use rexie::Rexie;
use rexie::TransactionMode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::JsValue;

use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::utils::js_value;

/// Stored value and the sole eviction-order metadata. Extra stored fields are ignored.
#[derive(Serialize, Deserialize)]
struct DataStruct<T> {
    /// Inline IndexedDB primary key.
    key: String,
    /// Per-key monotonic access timestamp used by the LRU index.
    last_visit_time: i64,
    /// Caller-owned serialized payload.
    data: T,
}

impl<T> DataStruct<T> {
    /// Create a `DataStruct` instance by key and data
    pub fn new(key: &str, data: T) -> Self {
        // Capture the insertion timestamp once for the eviction index.
        let time_now = crate::utils::get_epoch_ms_i64();
        Self {
            key: key.to_owned(),
            last_visit_time: time_now,
            data,
        }
    }
}

/// Compute the next per-key IndexedDB visit timestamp.
pub(crate) fn next_visit_time_after(previous: i64, now: i64) -> i64 {
    // Post: for previous < i64::MAX, result > previous. The prune index
    // remains monotonic per key under equal-ms or backward wall-clock readings.
    now.max(previous.saturating_add(1))
}

/// Touch an existing row without moving its timestamp backward.
fn next_visit_time(previous: i64) -> i64 {
    next_visit_time_after(previous, crate::utils::get_epoch_ms_i64())
}

/// Named IndexedDB key/value storage with bounded row capacity.
pub struct IdbStorage {
    /// Open database handle; transactions own their requests until completion.
    db: Rexie,
    /// Maximum number of stored rows.
    cap: u32,
    /// Object store and database name selected by the caller.
    storage_name: String,
}

impl IdbStorage {
    /// Open a named database with a nonzero maximum number of rows.
    ///
    /// `row_capacity` counts rows, and opening an existing store restores that row bound.
    /// Only a newly created database runs Rexie's schema callback. Reopening an
    /// existing database does not remove old indexes; they are unused. We neither
    /// bump a schema version nor delete caller data to remove that residue.
    pub async fn new_with_cap_and_name(row_capacity: u32, name: &str) -> Result<Self> {
        if row_capacity == 0 {
            return Err(Error::InvalidCapacity);
        }
        let storage = Self {
            db: Rexie::builder(name)
                .add_object_store(
                    ObjectStore::new(name)
                        .key_path("key")
                        .auto_increment(false)
                        .add_index(Index::new("last_visit_time", "last_visit_time")),
                )
                .build()
                .await
                .map_err(Error::IDBError)?,
            cap: row_capacity,
            storage_name: name.to_owned(),
        };
        // Opening under a smaller row budget immediately restores the configured bound.
        storage.prune().await?;
        Ok(storage)
    }

    /// Open the store together with its transaction completion witness.
    /// Writers must await `done` before reporting durable success.
    fn transaction(&self, mode: TransactionMode) -> Result<(rexie::Transaction, rexie::Store)> {
        // Keep the completion handle alongside the store used for requests.
        let transaction = self
            .db
            .transaction(&self.db.store_names(), mode)
            .map_err(Error::IDBError)?;
        // Requests are scoped to this database's configured object store.
        let store = transaction
            .store(self.storage_name.as_str())
            .map_err(Error::IDBError)?;
        Ok((transaction, store))
    }

    /// Restore the configured row budget by retiring every least-recently-accessed excess row.
    ///
    /// All count, selection, and deletion requests share one read-write transaction. The
    /// transaction is committed only after each candidate can be decoded and deleted.
    async fn prune(&self) -> Result<()> {
        let (tx, store) = self.transaction(TransactionMode::ReadWrite)?;
        let count = store.count(None).await.map_err(Error::IDBError)?;
        if count <= self.cap {
            tx.done().await.map_err(Error::IDBError)?;
            return Ok(());
        }
        let delete_count = count.saturating_sub(self.cap);

        let item_index = store.index("last_visit_time").map_err(Error::IDBError)?;
        let entries = item_index
            .get_all(None, Some(delete_count), None, None)
            .await
            .map_err(Error::IDBError)?;
        tracing::debug!("entries: {:?}", entries);

        // Validate every candidate before scheduling any deletion, so a decode error leaves
        // the transaction with no destructive requests queued.
        let keys = entries
            .into_iter()
            .map(|(_key, value)| {
                let data_entry: DataStruct<serde_json::Value> = js_value::deserialize(value)?;
                Ok(data_entry.key)
            })
            .collect::<Result<Vec<_>>>()?;
        for key in keys {
            store
                .delete(&JsValue::from(&key))
                .await
                .map_err(Error::IDBError)?;
        }
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    /// Delete all values.
    pub async fn clear(&self) -> Result<()> {
        let (tx, store) = self.transaction(TransactionMode::ReadWrite)?;
        store.clear().await.map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    /// Get the current storage usage.
    pub async fn count(&self) -> Result<u32> {
        let (_tx, store) = self.transaction(TransactionMode::ReadOnly)?;
        let count = store.count(None).await.map_err(Error::IDBError)?;
        Ok(count)
    }
}

#[async_trait(?Send)]
impl<V> KvStorageInterface<V> for IdbStorage
where V: DeserializeOwned + Serialize + Sized
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        // Reading is a touch: keep lookup and timestamp replacement in the same
        // write transaction so a concurrent put cannot be overwritten by stale data.
        let (tx, store) = self.transaction(TransactionMode::ReadWrite)?;
        // The inline primary key identifies the row protected by this transaction.
        let k: JsValue = JsValue::from(key);
        // Decode before scheduling any write so malformed values remain untouched.
        let v = store.get(&k).await.map_err(Error::IDBError)?;
        let v: Option<DataStruct<V>> = js_value::deserialize(&v)?;
        if let Some(mut v) = v {
            v.last_visit_time = next_visit_time(v.last_visit_time);
            store
                .put(&js_value::serialize(&v)?, None)
                .await
                .map_err(Error::IDBError)?;
            tx.done().await.map_err(Error::IDBError)?;
            return Ok(Some(v.data));
        }
        tx.done().await.map_err(Error::IDBError)?;
        Ok(None)
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        // Serialize before opening a destructive transaction so serialization failure cannot
        // evict any previously stored row.
        let replacement = js_value::serialize(&DataStruct::new(key, value))?;
        let (tx, store) = self.transaction(TransactionMode::ReadWrite)?;
        // Check existence under the same transaction that will evict and store the row.
        let existing = store
            .get(&JsValue::from(key))
            .await
            .map_err(Error::IDBError)?;
        if existing.is_undefined() || existing.is_null() {
            // Count includes every row visible to this transaction; only a new key needs room.
            let count = store.count(None).await.map_err(Error::IDBError)?;
            if count >= self.cap {
                // Remove every row needed to make room, rather than only the first candidate.
                let delete_count = count.saturating_sub(self.cap).saturating_add(1);
                let item_index = store.index("last_visit_time").map_err(Error::IDBError)?;
                let entries = item_index
                    .get_all(None, Some(delete_count), None, None)
                    .await
                    .map_err(Error::IDBError)?;
                // Decode every candidate before queuing deletions, avoiding partial work if a
                // malformed record prevents the operation from proceeding.
                let keys = entries
                    .into_iter()
                    .map(|(_entry_key, entry_value)| {
                        let data_entry: DataStruct<serde_json::Value> =
                            js_value::deserialize(entry_value)?;
                        Ok(data_entry.key)
                    })
                    .collect::<Result<Vec<_>>>()?;
                for entry_key in keys {
                    store
                        .delete(&JsValue::from(&entry_key))
                        .await
                        .map_err(Error::IDBError)?;
                }
            }
        }
        store
            .put(&replacement, None)
            .await
            .map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let (_tx, store) = self.transaction(TransactionMode::ReadOnly)?;
        let entries = store
            .get_all(None, None, None, None)
            .await
            .map_err(Error::IDBError)?;

        entries
            .iter()
            .map(|(k, v)| {
                let key = k
                    .as_string()
                    .ok_or_else(|| Error::JsError("IndexedDB key is not a string".to_string()))?;
                let data = js_value::deserialize::<DataStruct<V>>(v)?.data;
                Ok((key, data))
            })
            .collect()
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let (tx, store) = self.transaction(TransactionMode::ReadWrite)?;
        store.delete(&key.into()).await.map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        IdbStorage::clear(self).await
    }

    async fn count(&self) -> Result<u32> {
        IdbStorage::count(self).await
    }
}

impl std::fmt::Debug for IdbStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdbStorage")
            .field("storage_name", &self.storage_name)
            .field("cap", &self.cap)
            .finish()
    }
}

#[cfg(test)]
#[path = "idb/tests.rs"]
mod tests;
