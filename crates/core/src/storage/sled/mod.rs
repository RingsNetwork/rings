#![deny(missing_docs)]

//! Persistent native key-value storage: one file per key under a byte budget.
//!
//! Budget law: the bytes of every stored file sum to at most `capacity`. A `put` whose value
//! would exceed the budget first retires the least recently written *other* keys until the
//! value fits; a value larger than the whole budget is rejected with
//! `Error::StorageValueExceedsCapacity` and changes nothing. The budget is also restored when
//! the storage is opened, so lowering the configured capacity retires the oldest files.
//!
//! The in-memory index mirrors the directory: it is rebuilt from the directory on open and
//! updated by every write under the same lock, and the directory is owned exclusively by this
//! instance while it is open. A retired file that fails to delete is dropped from the index and
//! reclaimed by the next open.

use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;
use std::time::SystemTime;

use async_trait::async_trait;
use itertools::Itertools;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;

use super::write_ordered::WriteOrderedMap;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;

/// The on-disk state known to this instance: file lengths by file name, in write order.
#[derive(Debug, Default)]
struct FileIndex {
    files: WriteOrderedMap<u64>,
    used_bytes: u64,
}

impl FileIndex {
    /// Forget `name`, releasing its bytes from the budget.
    ///
    /// Post: `used_bytes` no longer counts `name`.
    fn forget(&mut self, name: &str) {
        if let Some(len) = self.files.remove(name) {
            self.used_bytes = self.used_bytes.saturating_sub(len);
        }
    }

    /// Record `name` as the most recently written file of `len` bytes.
    fn record(&mut self, name: String, len: u64) {
        self.forget(&name);
        self.files.insert(name, len);
        self.used_bytes = self.used_bytes.saturating_add(len);
    }

    /// Forget the least recently written file, returning its name.
    fn retire_oldest(&mut self) -> Option<String> {
        let (name, len) = self.files.pop_oldest()?;
        self.used_bytes = self.used_bytes.saturating_sub(len);
        Some(name)
    }

    fn forget_all(&mut self) {
        self.files.clear();
        self.used_bytes = 0;
    }
}

/// StorageInstance struct
pub struct SledStorage {
    root: PathBuf,
    capacity: u64,
    index: RwLock<FileIndex>,
}

impl SledStorage {
    /// New SledStorage
    /// * cap: max_size in bytes
    /// * path: db file location
    pub async fn new_with_cap_and_path<P>(cap: u32, path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        std::fs::create_dir_all(path.as_ref()).map_err(Error::ServiceIOError)?;
        let storage = Self {
            root: path.as_ref().to_path_buf(),
            capacity: u64::from(cap),
            index: RwLock::new(FileIndex::default()),
        };
        let mut index = storage.index.write().map_err(|_| Error::DHTSyncLockError)?;
        *index = storage.scan_directory()?;
        storage.retire_until_fits(&mut index, 0)?;
        drop(index);
        Ok(storage)
    }

    fn key_path(&self, key: &str) -> PathBuf {
        self.root.join(file_name_for(key))
    }

    /// Rebuild the index from the directory, ordering files by their modification time so the
    /// budget retires the oldest write first across restarts.
    fn scan_directory(&self) -> Result<FileIndex> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileIndex::default());
            }
            Err(error) => return Err(Error::ServiceIOError(error)),
        };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = entry_file_name(&path) else {
                continue;
            };
            let metadata = std::fs::metadata(&path).map_err(Error::ServiceIOError)?;
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((modified, name.to_owned(), metadata.len()));
        }
        files.sort();
        let mut index = FileIndex::default();
        for (_, name, len) in files {
            index.record(name, len);
        }
        Ok(index)
    }

    /// Retire least recently written files until `incoming` more bytes fit the budget.
    ///
    /// Pre: `incoming <= capacity`.
    /// Post: `index.used_bytes + incoming <= capacity`.
    fn retire_until_fits(&self, index: &mut FileIndex, incoming: u64) -> Result<()> {
        while index.used_bytes.saturating_add(incoming) > self.capacity {
            let Some(name) = index.retire_oldest() else {
                break;
            };
            remove_file_if_present(&self.root.join(name))?;
        }
        Ok(())
    }

    fn read_index(&self) -> Result<RwLockReadGuard<'_, FileIndex>> {
        self.index.read().map_err(|_| Error::DHTSyncLockError)
    }

    fn write_index(&self) -> Result<RwLockWriteGuard<'_, FileIndex>> {
        self.index.write().map_err(|_| Error::DHTSyncLockError)
    }
}

fn file_name_for(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::ServiceIOError(error)),
    }
}

#[async_trait]
impl<V> KvStorageInterface<V> for SledStorage
where V: Serialize + DeserializeOwned + Sync
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        let _guard = self.read_index()?;
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
        let data = rings_codec::serialize(&(key, value)).map_err(Error::CodecSerialize)?;
        let required = data.len() as u64;
        if required > self.capacity {
            return Err(Error::StorageValueExceedsCapacity {
                required,
                capacity: self.capacity,
            });
        }
        let mut index = self.write_index()?;
        let name = file_name_for(key);
        // The rewritten key does not compete with itself for the budget.
        index.forget(&name);
        self.retire_until_fits(&mut index, required)?;
        std::fs::create_dir_all(&self.root).map_err(Error::ServiceIOError)?;
        tracing::debug!("Try inserting key: {:?}", key);
        let path = self.root.join(&name);
        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, data).map_err(Error::ServiceIOError)?;
        std::fs::rename(tmp_path, path).map_err(Error::ServiceIOError)?;
        index.record(name, required);
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let index = self.read_index()?;
        Ok(index
            .files
            .iter()
            .flat_map(|(name, _)| {
                let data = std::fs::read(self.root.join(name)).ok()?;
                rings_codec::deserialize::<(String, V)>(&data).ok()
            })
            .collect_vec())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let mut index = self.write_index()?;
        remove_file_if_present(&self.key_path(key))?;
        index.forget(&file_name_for(key));
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        let mut index = self.write_index()?;
        for (name, _) in index.files.iter() {
            std::fs::remove_file(self.root.join(name)).map_err(Error::ServiceIOError)?;
        }
        index.forget_all();
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        Ok(self.read_index()?.files.len() as u32)
    }
}

fn entry_file_name(path: &Path) -> Option<&str> {
    let file_name = path.file_name().and_then(|name| name.to_str())?;
    (file_name.len() == 40 && file_name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .then_some(file_name)
}

impl std::fmt::Debug for SledStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SledStorage")
            .field("capacity", &self.capacity)
            .field("root", &self.root)
            .finish()
    }
}

#[cfg(test)]
mod test_sled;
