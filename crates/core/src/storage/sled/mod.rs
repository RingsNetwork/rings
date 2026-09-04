#![deny(missing_docs)]

//! Persistent native key-value storage: one file per key under a byte budget.
//!
//! Budget law: the bytes of every stored file sum to at most `capacity`. A `put` whose value
//! would exceed the budget first retires the least recently written *other* keys until the
//! value fits; a value larger than the whole budget is rejected with
//! `Error::StorageValueExceedsCapacity` and changes nothing. The budget is also restored when
//! the storage is opened, so lowering the configured capacity retires the oldest files.
//!
//! Index law: the in-memory index mirrors the directory. It is rebuilt from the directory on
//! open (stale `.tmp` files from an interrupted write are removed then), every write updates it
//! under the same lock only after the file system operation succeeded, and the directory is
//! owned exclusively by this instance while it is open.
//!
//! Decode law: the store holds only records decodable as `V`. A record the current schema
//! cannot decode (written by an earlier build) is retired on the read that discovers it and
//! reported absent, so it neither serves stale data nor occupies the budget.

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
        let mut index = storage.write_index()?;
        *index = storage.scan_directory()?;
        storage.retire_until_fits(&mut index, 0)?;
        drop(index);
        Ok(storage)
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
            if path.extension().is_some_and(|extension| extension == "tmp") {
                remove_file_if_present(&path)?;
                continue;
            }
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
        self.index.read().map_err(|_| Error::StorageLockPoisoned)
    }

    fn write_index(&self) -> Result<RwLockWriteGuard<'_, FileIndex>> {
        self.index.write().map_err(|_| Error::StorageLockPoisoned)
    }

    /// Remove the record stored as `name` and forget it.
    fn retire(&self, name: &str) -> Result<()> {
        let mut index = self.write_index()?;
        remove_file_if_present(&self.root.join(name))?;
        index.forget(name);
        Ok(())
    }

    /// Decode one record file, retiring it when the current schema cannot read it.
    fn decode_record<V>(&self, name: &str, data: &[u8]) -> Result<Option<(String, V)>>
    where V: DeserializeOwned {
        match rings_codec::deserialize::<(String, V)>(data) {
            Ok(record) => Ok(Some(record)),
            Err(_) => {
                self.retire(name)?;
                Ok(None)
            }
        }
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
        let name = file_name_for(key);
        let data = {
            let _guard = self.read_index()?;
            match std::fs::read(self.root.join(&name)) {
                Ok(data) => data,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(Error::ServiceIOError(error)),
            }
        };
        Ok(self
            .decode_record::<V>(&name, &data)?
            .filter(|(stored_key, _)| stored_key == key)
            .map(|(_, value)| value))
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        let data = rings_codec::serialize(&(key, value)).map_err(Error::CodecSerialize)?;
        let required = u64::try_from(data.len()).map_err(|_| Error::StorageCountOverflow)?;
        if required > self.capacity {
            return Err(Error::StorageValueExceedsCapacity {
                required,
                capacity: self.capacity,
            });
        }
        let name = file_name_for(key);
        let path = self.root.join(&name);
        let tmp_path = path.with_extension("tmp");
        let mut index = self.write_index()?;
        // The temporary file lives outside the index, so a failed write changes nothing.
        std::fs::create_dir_all(&self.root).map_err(Error::ServiceIOError)?;
        std::fs::write(&tmp_path, data).map_err(Error::ServiceIOError)?;
        tracing::debug!("Try inserting key: {:?}", key);
        // The rewritten key does not compete with itself for the budget.
        let previous_len = index.files.get(&name).copied();
        index.forget(&name);
        self.retire_until_fits(&mut index, required)?;
        if let Err(error) = std::fs::rename(&tmp_path, &path) {
            // The previous record is still on disk; keep the index faithful to it.
            if let Some(previous_len) = previous_len {
                index.record(name, previous_len);
            }
            remove_file_if_present(&tmp_path)?;
            return Err(Error::ServiceIOError(error));
        }
        index.record(name, required);
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let records = {
            let index = self.read_index()?;
            index
                .files
                .iter()
                .map(|(name, _)| (name.to_owned(), std::fs::read(self.root.join(name))))
                .collect_vec()
        };
        let mut decoded = Vec::with_capacity(records.len());
        for (name, data) in records {
            let data = match data {
                Ok(data) => data,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::ServiceIOError(error)),
            };
            if let Some(record) = self.decode_record::<V>(&name, &data)? {
                decoded.push(record);
            }
        }
        Ok(decoded)
    }

    async fn remove(&self, key: &str) -> Result<()> {
        self.retire(&file_name_for(key))
    }

    async fn clear(&self) -> Result<()> {
        let mut index = self.write_index()?;
        while let Some(name) = index.retire_oldest() {
            remove_file_if_present(&self.root.join(name))?;
        }
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        let count = self.read_index()?.files.len();
        u32::try_from(count).map_err(|_| Error::StorageCountOverflow)
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
