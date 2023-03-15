#![warn(missing_docs)]
#![allow(clippy::ptr_offset_with_cast)]
//! Persistence Storage for default, use `sled` as backend db.
use std::str::FromStr;

use async_trait::async_trait;
use itertools::Itertools;
use serde::de::DeserializeOwned;
use sled;

use super::PersistenceStorageOperation;
use super::PersistenceStorageReadAndWrite;
use super::PersistenceStorageRemove;
use crate::err::Error;
use crate::err::Result;

trait KvStorageBasic {
    fn get_db(&self) -> &sled::Db;
}

/// StorageInstance struct
#[allow(dead_code)]
pub struct KvStorage {
    db: sled::Db,
    cap: usize,
    path: String,
}

impl KvStorage {
    /// New KvStorage
    /// * cap: max_size in bytes
    /// * path: db file location
    pub async fn new_with_cap_and_path<P>(cap: usize, path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        let db = sled::Config::new()
            .path(path.as_ref())
            .mode(sled::Mode::HighThroughput)
            .cache_capacity(cap as u64)
            .open()
            .map_err(Error::SledError)?;
        Ok(Self {
            db,
            cap,
            path: path.as_ref().to_string_lossy().to_string(),
        })
    }

    /// New KvStorage
    /// * cap: max_size in bytes
    /// * name: db file location
    pub async fn new_with_cap_and_name(cap: usize, name: &str) -> Result<Self> {
        Self::new_with_cap_and_path(cap, name).await
    }

    /// New KvStorage with default path
    /// default_path is `./`
    pub async fn new_with_cap(cap: usize) -> Result<Self> {
        Self::new_with_cap_and_path(cap, "./data").await
    }

    /// New KvStorage with default capacity and specific path
    /// * path: db file location
    pub async fn new_with_path<P>(path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        Self::new_with_cap_and_path(200000000, path).await
    }

    /// New KvStorage
    /// * default capacity 200 megabytes
    /// * default path `./data`
    pub async fn new() -> Result<Self> {
        Self::new_with_cap(200000000).await
    }

    /// Delete current KvStorage
    #[cfg(test)]
    pub async fn delete(self) -> Result<()> {
        let path = self.path.clone();
        drop(self);
        tokio::fs::remove_dir_all(path.as_str())
            .await
            .map_err(Error::IOError)?;
        Ok(())
    }

    /// Generate a random path
    pub fn random_path(prefix: &str) -> String {
        let p = std::path::Path::new(prefix).join(uuid::Uuid::new_v4().to_string());
        p.to_string_lossy().to_string()
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
        Ok(())
    }

    async fn close(self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl<K, V, I> PersistenceStorageReadAndWrite<K, V> for I
where
    K: ToString + FromStr + std::marker::Sync + Send,
    V: DeserializeOwned + serde::Serialize + std::marker::Sync + Send,
    I: PersistenceStorageOperation + std::marker::Sync + KvStorageBasic,
{
    /// Get a cache entry by `key`.
    async fn get(&self, key: &K) -> Result<Option<V>> {
        let k = key.to_string();
        let k = k.as_bytes();
        let v = self.get_db().get(k).map_err(Error::SledError)?;
        if let Some(v) = v {
            let v = v.as_ref();
            return bincode::deserialize(v)
                .map_err(Error::BincodeDeserialize)
                .map(|r| Some(r));
        }
        Ok(None)
    }

    /// Put `entry` in the cache under `key`.
    async fn put(&self, key: &K, value: &V) -> Result<()> {
        self.prune().await?;
        let data = bincode::serialize(value).map_err(Error::BincodeSerialize)?;
        println!("insert v: {:?}", data);
        self.get_db()
            .insert(key.to_string().as_bytes(), data)
            .map_err(Error::SledError)?;
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(K, V)>> {
        let iter = self.get_db().iter();
        Ok(iter
            .flatten()
            .flat_map(|(k, v)| {
                Some((
                    K::from_str(std::str::from_utf8(k.as_ref()).ok()?).ok()?,
                    bincode::deserialize(v.as_ref()).ok()?,
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

impl std::fmt::Debug for KvStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvStorage")
            .field("cap", &self.cap)
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(test)]
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
        let storage = KvStorage::new_with_cap_and_path(4096, "temp/db")
            .await
            .unwrap();
        let key1 = "test1".to_owned();
        let data1 = TestStorageStruct {
            content: "test1".to_string(),
        };
        storage.put(&key1, &data1).await.unwrap();
        let count1 = storage.count().await.unwrap();
        assert!(count1 == 1, "expect count1.1 is {}, got {}", 1, count1);
        let got_v1: TestStorageStruct = storage.get(&key1).await.unwrap().unwrap();
        assert!(
            got_v1.content.eq(data1.content.as_str()),
            "expect value1 is {}, got {}",
            data1.content,
            got_v1.content
        );

        let key2 = "test2".to_owned();
        let data2 = TestStorageStruct {
            content: "test2".to_string(),
        };

        storage.put(&key2, &data2).await.unwrap();

        let count_got_2 = storage.count().await.unwrap();
        assert!(count_got_2 == 2, "expect count 2, got {}", count_got_2);

        let all_entries: Vec<(String, TestStorageStruct)> = storage.get_all().await.unwrap();
        assert!(
            all_entries.len() == 2,
            "all_entries len expect 2, got {}",
            all_entries.len()
        );

        let keys = vec![key1, key2];
        let values = vec![data1.content, data2.content];

        assert!(
            all_entries
                .iter()
                .any(|(k, v)| { keys.contains(k) && values.contains(&v.content) }),
            "not found items"
        );
        let data3: u64 = 101;
        let key3 = "key3".to_owned();
        storage.put(&key3, &data3).await.unwrap();
        let got_d3: u64 = storage.get(&key3).await.unwrap().unwrap();
        assert!(data3 == got_d3, "expect {}, got {}", data3, got_d3);
        storage.clear().await.unwrap();
        let count1 = storage.count().await.unwrap();
        assert!(count1 == 0, "expect count1.2 is {}, got {}", 0, count1);
        storage.get_db().flush_async().await.unwrap();
        drop(storage)
    }
}
