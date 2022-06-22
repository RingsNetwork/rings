#![warn(missing_docs)]

//! Storage for wasm
use std::mem::size_of_val;
use std::ops::Add;
use std::ops::Sub;

use async_trait::async_trait;
use rexie::Index;
use rexie::ObjectStore;
use rexie::Rexie;
use rexie::TransactionMode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::JsValue;

use super::PersistenceStorageOperation;
use super::PersistenceStorageReadAndWrite;
use super::PersistenceStorageRemove;
use crate::err::Error;
use crate::err::Result;

const REXIE_STORE_NAME: &str = "rings-storage";

/// DataStruct of IndexedDB store entry
#[derive(Serialize, Deserialize)]
struct DataStruct<T> {
    key: String,
    last_visit_time: i64,
    visit_count: u64,
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
pub struct IDBStorage {
    db: Rexie,
    cap: usize,
}

/// IDBStorage basic functions
pub trait IDBStorageBasic {
    /// Get transaction and store of current `ObjectStore`
    fn get_tx_store(&self, mode: TransactionMode) -> Result<(rexie::Transaction, rexie::Store)>;
}

impl IDBStorage {
    /// New IDBStorage
    /// * cap: rows of data limit
    pub async fn new_with_cap(cap: usize) -> Result<Self> {
        if cap == 0 {
            return Err(Error::InvalidCapacity);
        }
        Ok(Self {
            db: Rexie::builder("rings-storage")
                .add_object_store(
                    ObjectStore::new(REXIE_STORE_NAME)
                        .key_path("key")
                        .auto_increment(false)
                        .add_index(Index::new("last_visit_time", "last_visit_time"))
                        .add_index(Index::new("visit_count", "visit_count")),
                )
                .build()
                .await
                .map_err(Error::IDBError)?,
            cap,
        })
    }

    /// New IDBStorage with default capacity 50000 rows data limit
    pub async fn new() -> Result<Self> {
        Self::new_with_cap(50000).await
    }
}

impl IDBStorageBasic for IDBStorage {
    fn get_tx_store(&self, mode: TransactionMode) -> Result<(rexie::Transaction, rexie::Store)> {
        let transaction = self
            .db
            .transaction(&[REXIE_STORE_NAME], mode)
            .map_err(Error::IDBError)?;
        let store = transaction
            .store(REXIE_STORE_NAME)
            .map_err(Error::IDBError)?;
        Ok((transaction, store))
    }
}

#[async_trait(?Send)]
impl<K, V, I> PersistenceStorageReadAndWrite<K, V> for I
where
    K: ToString + From<String>,
    V: DeserializeOwned + Serialize + Sized,
    I: PersistenceStorageOperation + IDBStorageBasic,
{
    async fn get(&self, key: &K) -> Result<V> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        let k: JsValue = JsValue::from(key.to_string());
        let v = store.get(&k).await.map_err(Error::IDBError)?;
        let mut v: DataStruct<V> = v.into_serde().map_err(Error::Deserialize)?;
        v.last_visit_time = chrono::Utc::now().timestamp_millis();
        v.visit_count += 1;
        store
            .put(
                &JsValue::from_serde(&v).map_err(Error::Serialize)?,
                //Some(&k),
                None,
            )
            .await
            .map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(v.data)
    }

    async fn get_all(&self) -> Result<Vec<(K, V)>> {
        let (_tx, store) = self.get_tx_store(TransactionMode::ReadOnly)?;
        let entries = store
            .get_all(None, None, None, None)
            .await
            .map_err(Error::IDBError)?;
        Ok(entries
            .iter()
            .map(|(k, v)| {
                (
                    K::from(k.as_string().unwrap()),
                    v.into_serde::<DataStruct<V>>().unwrap().data,
                )
            })
            .collect::<Vec<(K, V)>>())
    }

    async fn put(&self, key: &K, entry: &V) -> Result<()> {
        self.prune().await?;
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        store
            .put(
                &JsValue::from_serde(&DataStruct::new(key.to_string().as_str(), entry))
                    .map_err(Error::Serialize)?,
                //Some(&key.into()),
                None,
            )
            .await
            .map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl<K, I> PersistenceStorageRemove<K> for I
where
    K: ToString,
    I: PersistenceStorageOperation + IDBStorageBasic,
{
    async fn remove(&self, key: &K) -> Result<()> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        store
            .delete(&key.to_string().into())
            .await
            .map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl PersistenceStorageOperation for IDBStorage {
    async fn clear(&self) -> Result<()> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        store.clear().await.map_err(Error::IDBError)?;
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }

    async fn count(&self) -> Result<u64> {
        let (_tx, store) = self.get_tx_store(TransactionMode::ReadOnly)?;
        let count = store.count(None).await.map_err(Error::IDBError)?;
        Ok(count as u64)
    }

    async fn max_size(&self) -> Result<usize> {
        Ok(self.cap)
    }

    async fn total_size(&self) -> Result<usize> {
        let (_tx, store) = self.get_tx_store(TransactionMode::ReadOnly)?;
        let entries = store
            .get_all(None, None, None, None)
            .await
            .map_err(Error::IDBError)?;
        let size = entries
            .iter()
            .map(|(k, v)| size_of_val(k) + size_of_val(v))
            .sum::<usize>();
        Ok(size)
    }

    async fn prune(&self) -> Result<()> {
        let (tx, store) = self.get_tx_store(TransactionMode::ReadWrite)?;
        let count = store.count(None).await.map_err(Error::IDBError)? as usize;
        if count < self.cap {
            return Ok(());
        }
        let delete_count = count.sub(self.cap).add(1);
        let delete_count = u32::try_from(delete_count).unwrap_or(0);
        if delete_count == 0 {
            return Ok(());
        }

        let item_index = store.index("last_visit_time").map_err(Error::IDBError)?;
        let entries = item_index
            .get_all(None, Some(delete_count), None, None)
            .await
            .map_err(Error::IDBError)?;
        log::debug!("entries: {:?}", entries);
        if let Some((_k, value)) = entries.first() {
            let data_entry: DataStruct<serde_json::Value> =
                value.into_serde().map_err(Error::Serialize)?;
            store
                .delete(&JsValue::from(&data_entry.key))
                .await
                .map_err(Error::IDBError)?;
        }
        tx.done().await.map_err(Error::IDBError)?;
        Ok(())
    }
}
