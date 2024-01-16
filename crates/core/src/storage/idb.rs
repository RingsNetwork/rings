#![warn(missing_docs)]

//! Storage for wasm
use std::ops::Add;
use std::ops::Sub;

use async_trait::async_trait;
use itertools::Itertools;
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

/// Default IndexedDB database and storage name
pub const DEFAULT_REXIE_STORE_NAME: &str = "rings-storage";

/// DataStruct of IndexedDB store entry
#[derive(Serialize, Deserialize)]
struct DataStruct<T> {
    key: String,
    last_visit_time: i64,
    visit_count: u32,
    created_time: i64,
    data: T,
}

impl<T> DataStruct<T> {
    /// Create a `DataStruct` instance by key and data
    pub fn new(key: &str, data: T) -> Self {
        let time_now = chrono::Utc::now().timestamp_millis();
        Self {
            key: key.to_owned(),
            last_visit_time: time_now,
            created_time: time_now,
            visit_count: 0,
            data,
        }
    }
}

/// StorageInstance struct
pub struct IdbStorage {
    db: Rexie,
    cap: u32,
    storage_name: String,
}

impl IdbStorage {
    /// New IdbStorage
    /// * cap: rows of data limit
    pub async fn new_with_cap(cap: u32) -> Result<Self> {
        Self::new_with_cap_and_name(cap, DEFAULT_REXIE_STORE_NAME).await
    }

    /// New IdbStorage with capacity and name
    pub async fn new_with_cap_and_name(cap: u32, name: &str) -> Result<Self> {
        if cap == 0 {
            return Err(Error::InvalidCapacity);
        }
        Ok(Self {
            db: Rexie::builder(name)
                .add_object_store(
                    ObjectStore::new(name)
                        .key_path("key")
                        .auto_increment(false)
                        .add_index(Index::new("last_visit_time", "last_visit_time"))
                        .add_index(Index::new("visit_count", "visit_count")),
                )
                .build()
                .await
                .map_err(Error::IDBError)?,
            cap,
            storage_name: name.to_owned(),
        })
    }

    /// Get db transaction and store
    pub fn get_tx_store(
        &self,
        mode: TransactionMode,
    ) -> Result<(rexie::Transaction, rexie::Store)> {
        let transaction = self
            .db
            .transaction(&self.db.store_names(), mode)
            .map_err(Error::IDBError)?;
        let store = transaction
            .store(self.storage_name.as_str())
            .map_err(Error::IDBError)?;
        Ok((transaction, store))
    }

    async fn prune(&self) -> Result<()> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        let count = store.count(None).await.map_err(Error::IDBError)?;
        if count < self.cap {
            return Ok(());
        }
        let delete_count = count.sub(self.cap).add(1);
        if delete_count == 0 {
            return Ok(());
        }

        let item_index = store.index("last_visit_time").map_err(Error::IDBError)?;
        let entries = item_index
            .get_all(None, Some(delete_count), None, None)
            .await
            .map_err(Error::IDBError)?;
        tracing::debug!("entries: {:?}", entries);

        if let Some((_k, value)) = entries.first() {
            let data_entry: DataStruct<serde_json::Value> = js_value::deserialize(value)?;
            store
                .delete(&JsValue::from(&data_entry.key))
                .await
                .map_err(Error::IDBError)?;
        }
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    /// Delete all values.
    pub async fn clear(&self) -> Result<()> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        store.clear().await.map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    /// Get the current storage usage.
    pub async fn count(&self) -> Result<u32> {
        let (_tx, store) = self.get_tx_store(TransactionMode::ReadOnly)?;
        let count = store.count(None).await.map_err(Error::IDBError)?;
        Ok(count)
    }
}

#[async_trait(?Send)]
impl<V> KvStorageInterface<V> for IdbStorage
where V: DeserializeOwned + Serialize + Sized
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        let k: JsValue = JsValue::from(key);
        let v = store.get(&k).await.map_err(Error::IDBError)?;
        let v: Option<DataStruct<V>> = js_value::deserialize(&v)?;
        if let Some(mut v) = v {
            v.last_visit_time = chrono::Utc::now().timestamp_millis();
            v.visit_count += 1;
            store
                .put(
                    &js_value::serialize(&v)?,
                    //Some(&k),
                    None,
                )
                .await
                .map_err(Error::IDBError)?;
            tx.done().await.map_err(Error::IDBError)?;
            return Ok(Some(v.data));
        }
        Ok(None)
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        self.prune().await?;
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        store
            .put(&js_value::serialize(&DataStruct::new(key, value))?, None)
            .await
            .map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let (_tx, store) = self.get_tx_store(TransactionMode::ReadOnly)?;
        let entries = store
            .get_all(None, None, None, None)
            .await
            .map_err(Error::IDBError)?;

        Ok(entries
            .iter()
            .map(|(k, v)| {
                (
                    k.as_string().unwrap(),
                    js_value::deserialize::<DataStruct<V>>(v).unwrap().data,
                )
            })
            .collect_vec())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
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
