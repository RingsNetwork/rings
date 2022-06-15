#![warn(missing_docs)]
#![allow(clippy::ptr_offset_with_cast)]
//! Persistence Storage for default, use `sled` as backend db.
use std::ops::Add;
use std::ops::Sub;

use arrayref::array_refs;
use async_trait::async_trait;
use chrono;
use itertools::Itertools;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sled;

use super::PersistenceStorageOperation;
use super::PersistenceStorageReadAndWrite;
use super::PersistenceStorageRemove;
use crate::err::Error;
use crate::err::Result;

/// DataStruct of KV store entry
struct DataStruct {
    last_visit_time: i64,
    visit_count: u64,
    created_time: i64,
    data: Vec<u8>,
}

trait KvStorageBasic {
    fn get_db(&self) -> &sled::Db;
}

impl DataStruct {
    /// Create a `DataStruct` instance by key and data
    pub fn new(data: Vec<u8>) -> Self {
        let time_now = chrono::Utc::now().timestamp_millis();
        Self {
            last_visit_time: time_now,
            created_time: time_now,
            visit_count: 0,
            data,
        }
    }

    /// Create DataStruct instance from bytes
    pub fn from_bytes(data: &[u8]) -> Self {
        let (fixed_data, content_data) = array_refs![data, 24;..;];
        let (last_visit_time, visit_count, created_time) = array_refs![fixed_data, 8, 8, 8];
        let last_visit_time = i64::from_le_bytes(*last_visit_time);
        let visit_count = u64::from_le_bytes(*visit_count);
        let created_time = i64::from_le_bytes(*created_time);
        let data = content_data.to_vec();
        Self {
            last_visit_time,
            visit_count,
            created_time,
            data,
        }
    }

    /// Convert DataStruct instance to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut data_vec: Vec<u8> = Vec::new();
        data_vec.extend_from_slice(&self.last_visit_time.to_le_bytes());
        data_vec.extend_from_slice(&self.visit_count.to_le_bytes());
        data_vec.extend_from_slice(&self.created_time.to_le_bytes());
        data_vec.extend_from_slice(&self.data);
        data_vec
    }

    /// Deserialize data of DataStruct to `T`
    pub fn deserialize_data<T>(&self) -> Result<T>
    where T: DeserializeOwned {
        bincode::deserialize(&self.data).map_err(Error::BincodeDeserialize)
    }

    /// Serialize data to `Vec<u8>`
    pub fn serialize_data<T>(data: &T) -> Result<Vec<u8>>
    where T: Serialize {
        bincode::serialize(data).map_err(Error::BincodeSerialize)
    }
}

/// StorageInstance struct
pub struct KvStorage {
    db: sled::Db,
    cap: usize,
}

impl KvStorage {
    /// New KvStorage
    /// * cap: max_size storage can use
    /// * path: db file location
    pub async fn new_with_cap_and_path<P>(cap: usize, path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        let db = sled::open(path).map_err(Error::SledError)?;
        Ok(Self { db, cap })
    }

    /// New KvStorage with default path
    /// default_path is `./`
    pub async fn new_with_cap(cap: usize) -> Result<Self> {
        Self::new_with_cap_and_path(cap, "./data").await
    }

    /// New KvStorage
    /// * default capacity 50000 rows data
    /// * default path `./data`
    pub async fn new() -> Result<Self> {
        Self::new_with_cap(50000).await
    }
}

impl KvStorageBasic for KvStorage {
    fn get_db(&self) -> &sled::Db {
        &self.db
    }
}

#[async_trait]
impl PersistenceStorageOperation for KvStorage {
    async fn clear(&self) -> Result<()> {
        self.db.clear().map_err(Error::SledError)?;
        // self.db.flush_async().await.map_err(Error::SledError)?;
        Ok(())
    }

    async fn count(&self) -> Result<u64> {
        Ok(self.db.len() as u64)
    }

    async fn max_size(&self) -> Result<usize> {
        Ok(self.cap)
    }

    /// Get the storage size, if applicable.
    async fn total_size(&self) -> Result<usize> {
        Ok(self.db.len())
    }

    /// Prune database storage
    async fn prune(&self) -> Result<()> {
        let db = self.get_db();
        let count = db.len();
        if count < self.cap {
            return Ok(());
        }
        let delete_count = count.sub(self.cap).add(1);
        let mut keys = db
            .iter()
            .flatten()
            .map(|(k, v)| {
                let (ltv, _) = array_refs![v.as_ref(), 8;..;];
                (k, i64::from_le_bytes(*ltv))
            })
            .sorted_by(|a, b| {
                if a.1.gt(&b.1) {
                    std::cmp::Ordering::Greater
                } else if a.1.lt(&b.1) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .collect_vec();
        keys.truncate(delete_count);
        let mut batch = sled::Batch::default();
        for (key, _) in keys {
            batch.remove(key);
        }
        db.apply_batch(batch).map_err(Error::SledError)?;
        Ok(())
    }
}

#[async_trait]
impl<K, V, I> PersistenceStorageReadAndWrite<K, V> for I
where
    K: ToString + From<String> + std::marker::Sync + Send,
    V: DeserializeOwned + serde::Serialize + std::marker::Sync + Send,
    I: PersistenceStorageOperation + std::marker::Sync + KvStorageBasic,
{
    /// Get a cache entry by `key`.
    async fn get(&self, key: &K) -> Result<V> {
        let k = key.to_string();
        let k = k.as_bytes();
        let v = self
            .get_db()
            .get(k)
            .map_err(Error::SledError)?
            .ok_or(Error::EntryNotFound)?;

        let mut value: DataStruct = DataStruct::from_bytes(v.as_ref());
        value.last_visit_time = chrono::Utc::now().timestamp_millis();
        value.visit_count += 1;
        self.get_db()
            .insert(k, value.to_bytes())
            .map_err(Error::SledError)?;

        value.deserialize_data()
    }

    /// Put `entry` in the cache under `key`.
    async fn put(&self, key: &K, value: &V) -> Result<()> {
        self.prune().await?;
        let data = DataStruct::serialize_data(value)?;
        self.get_db()
            .insert(key.to_string().as_bytes(), DataStruct::new(data).to_bytes())
            .map_err(Error::SledError)?;
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(K, V)>> {
        let iter = self.get_db().iter();
        Ok(iter
            .flatten()
            .flat_map(|(k, v)| {
                Some((
                    K::from(std::str::from_utf8(k.as_ref()).ok()?.to_string()),
                    serde_json::from_slice(v.as_ref()).ok()?,
                ))
            })
            .collect_vec())
    }
}

#[async_trait]
impl<K, I> PersistenceStorageRemove<K> for I
where
    K: ToString + std::marker::Sync,
    I: PersistenceStorageOperation + std::marker::Sync + KvStorageBasic,
{
    async fn remove(&self, key: &K) -> Result<()> {
        self.get_db()
            .remove(key.to_string().as_bytes())
            .map_err(Error::SledError)?;
        Ok(())
    }
}

#[cfg(test)]
#[cfg(feature = "default")]
mod test {
    use serde::Deserialize;
    use serde::Serialize;

    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    struct TestStorageStruct {
        content: String,
    }

    #[tokio::test]
    async fn test_kv_storage_put_delete() {
        let storage = KvStorage::new_with_cap_and_path(10, "temp/db")
            .await
            .unwrap();
        let key1 = "test1".to_owned();
        let data1 = TestStorageStruct {
            content: "test1".to_string(),
        };
        storage.put(&key1, &data1).await.unwrap();
        let count1 = storage.count().await.unwrap();
        assert!(count1 == 1, "expect count1.1 is {}, got {}", 1, count1);
        let got_v1: TestStorageStruct = storage.get(&key1).await.unwrap();
        assert!(
            got_v1.content.eq(data1.content.as_str()),
            "expect value1 is {}, got {}",
            data1.content,
            got_v1.content
        );

        let value1: DataStruct = DataStruct::from_bytes(
            storage
                .get_db()
                .get(key1.as_bytes())
                .unwrap()
                .unwrap()
                .as_ref(),
        );
        assert!(
            value1.visit_count == 1,
            "key1 visit_count, got {}, expect {}",
            value1.visit_count,
            1
        );

        storage.clear().await.unwrap();
        let count1 = storage.count().await.unwrap();
        assert!(count1 == 0, "expect count1.2 is {}, got {}", 0, count1);
    }

    fn create_test_data(count: usize) -> Vec<(String, TestStorageStruct)> {
        let mut data = Vec::new();
        for n in 0..count {
            data.push((format!("key_{}", n), TestStorageStruct {
                content: format!("test_data_{}", n),
            }))
        }
        data
    }

    #[tokio::test]
    async fn test_kv_storage_auto_prune() {
        let storage = KvStorage::new_with_cap_and_path(4, "temp/db")
            .await
            .unwrap();
        storage.clear().await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 0);
        let test_data = create_test_data(6);

        for n in 0..4 {
            let (k, v) = test_data.get(n).unwrap();
            storage.put(k, v).await.unwrap();
        }

        assert_eq!(storage.count().await.unwrap(), 4);

        let _: TestStorageStruct = storage.get(&test_data[1].0).await.unwrap();
        let _: TestStorageStruct = storage.get(&test_data[0].0).await.unwrap();

        storage.prune().await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 3);

        let _: TestStorageStruct = storage.get(&test_data[3].0).await.unwrap();

        let test_data_5 = test_data.get(5).unwrap();
        storage.put(&test_data_5.0, &test_data_5.1).await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 4);

        let all_entries: Vec<(String, TestStorageStruct)> = storage.get_all().await.unwrap();
        let (key2, _data2) = test_data.get(2).unwrap();
        let test_data_any = all_entries.iter().any(|(k, _v)| k.eq(key2));
        assert!(!test_data_any, "expect test_data_2 removed");

        storage.clear().await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 0);
    }
}
