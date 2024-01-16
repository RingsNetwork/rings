#![warn(missing_docs)]

//! Persistence Storage for default, use `sled` as backend db.

use async_trait::async_trait;
use itertools::Itertools;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;

/// StorageInstance struct
#[allow(dead_code)]
pub struct SledStorage {
    db: sled::Db,
    cap: u32,
    path: String,
}

impl SledStorage {
    /// New SledStorage
    /// * cap: max_size in bytes
    /// * path: db file location
    pub async fn new_with_cap_and_path<P>(cap: u32, path: P) -> Result<Self>
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
}

#[async_trait]
impl<V> KvStorageInterface<V> for SledStorage
where V: Serialize + DeserializeOwned + Sync
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        let v = self.db.get(key).map_err(Error::SledError)?;
        if let Some(v) = v {
            let v = v.as_ref();
            return bincode::deserialize(v)
                .map_err(Error::BincodeDeserialize)
                .map(|r| Some(r));
        }
        Ok(None)
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        let data = bincode::serialize(&value).map_err(Error::BincodeSerialize)?;
        tracing::debug!("Try inserting key: {:?}", key);
        self.db.insert(key, data).map_err(Error::SledError)?;
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let iter = self.db.iter();
        Ok(iter
            .flatten()
            .flat_map(|(k, v)| {
                Some((
                    std::str::from_utf8(k.as_ref()).ok()?.to_string(),
                    bincode::deserialize(v.as_ref()).ok()?,
                ))
            })
            .collect_vec())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        self.db
            .remove(key.to_string().as_bytes())
            .map_err(Error::SledError)?;
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        self.db.clear().map_err(Error::SledError)?;
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        Ok(self.db.len() as u32)
    }
}

impl std::fmt::Debug for SledStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SledStorage")
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
        let storage = SledStorage::new_with_cap_and_path(4096, "tmp/test_db")
            .await
            .unwrap();
        let key1 = "test1".to_owned();
        let data1 = TestStorageStruct {
            content: "test1".to_string(),
        };
        storage.put(&key1, &data1).await.unwrap();
        let count1 =
            <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
                .await
                .unwrap();
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

        let count_got_2 =
            <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
                .await
                .unwrap();
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

        // Clear full db and check if it's count is zero now.
        <SledStorage as KvStorageInterface<TestStorageStruct>>::clear::<'_, '_>(&storage)
            .await
            .unwrap();
        let count1 =
            <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
                .await
                .unwrap();
        assert!(count1 == 0, "expect count1 is 0, got {count1}");
        storage.db.flush_async().await.unwrap();

        drop(storage)
    }
}
