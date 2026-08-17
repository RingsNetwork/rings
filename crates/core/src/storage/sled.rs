#![deny(missing_docs)]

//! Persistent native key-value storage.

use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;

use async_trait::async_trait;
use itertools::Itertools;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;

use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;

/// StorageInstance struct
#[allow(dead_code)]
pub struct SledStorage {
    root: PathBuf,
    lock: RwLock<()>,
    cap: u32,
    path: String,
}

impl SledStorage {
    /// New SledStorage
    /// * cap: max_size in bytes
    /// * path: db file location
    pub async fn new_with_cap_and_path<P>(cap: u32, path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        std::fs::create_dir_all(path.as_ref()).map_err(Error::ServiceIOError)?;
        Ok(Self {
            root: path.as_ref().to_path_buf(),
            lock: RwLock::new(()),
            cap,
            path: path.as_ref().to_string_lossy().to_string(),
        })
    }

    fn key_path(&self, key: &str) -> PathBuf {
        let mut hasher = Sha1::new();
        hasher.update(key.as_bytes());
        self.root.join(hex::encode(hasher.finalize()))
    }

    fn entries(&self) -> Result<Vec<PathBuf>> {
        match std::fs::read_dir(&self.root) {
            Ok(entries) => Ok(entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| is_entry_path(path))
                .collect_vec()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(Error::ServiceIOError(error)),
        }
    }
}

#[async_trait]
impl<V> KvStorageInterface<V> for SledStorage
where V: Serialize + DeserializeOwned + Sync
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        let _guard = self.lock.read().map_err(|_| Error::DHTSyncLockError)?;
        match std::fs::read(self.key_path(key)) {
            Ok(data) => {
                let (stored_key, value): (String, V) =
                    rings_codec::deserialize(&data).map_err(Error::CodecDeserialize)?;
                if stored_key == key {
                    Ok(Some(value))
                } else {
                    Ok(None)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::ServiceIOError(error)),
        }
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        let _guard = self.lock.write().map_err(|_| Error::DHTSyncLockError)?;
        std::fs::create_dir_all(&self.root).map_err(Error::ServiceIOError)?;
        let data = rings_codec::serialize(&(key, value)).map_err(Error::CodecSerialize)?;
        tracing::debug!("Try inserting key: {:?}", key);
        let path = self.key_path(key);
        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, data).map_err(Error::ServiceIOError)?;
        std::fs::rename(tmp_path, path).map_err(Error::ServiceIOError)?;
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let _guard = self.lock.read().map_err(|_| Error::DHTSyncLockError)?;
        Ok(self
            .entries()?
            .into_iter()
            .flat_map(|path| {
                let data = std::fs::read(path).ok()?;
                rings_codec::deserialize::<(String, V)>(&data).ok()
            })
            .collect_vec())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let _guard = self.lock.write().map_err(|_| Error::DHTSyncLockError)?;
        match std::fs::remove_file(self.key_path(key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::ServiceIOError(error)),
        }
    }

    async fn clear(&self) -> Result<()> {
        let _guard = self.lock.write().map_err(|_| Error::DHTSyncLockError)?;
        for path in self.entries()? {
            std::fs::remove_file(path).map_err(Error::ServiceIOError)?;
        }
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        let _guard = self.lock.read().map_err(|_| Error::DHTSyncLockError)?;
        Ok(self.entries()?.len() as u32)
    }
}

fn is_entry_path(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    file_name.len() == 40 && file_name.as_bytes().iter().all(u8::is_ascii_hexdigit)
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
        let path = std::env::temp_dir().join(format!(
            "rings-file-kv-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let storage = SledStorage::new_with_cap_and_path(4096, &path)
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
        assert!(count_got_2 == 2, "expect count 2, got {count_got_2}");

        let all_entries: Vec<(String, TestStorageStruct)> = storage.get_all().await.unwrap();
        assert!(
            all_entries.len() == 2,
            "all_entries len expect 2, got {}",
            all_entries.len()
        );

        let keys = [key1, key2];
        let values = [data1.content, data2.content];

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
        assert!(data3 == got_d3, "expect {data3}, got {got_d3}");

        // Clear full db and check if it's count is zero now.
        <SledStorage as KvStorageInterface<TestStorageStruct>>::clear::<'_, '_>(&storage)
            .await
            .unwrap();
        let count1 =
            <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
                .await
                .unwrap();
        assert!(count1 == 0, "expect count1 is 0, got {count1}");

        drop(storage);
        let _ = std::fs::remove_dir_all(path);
    }
}
