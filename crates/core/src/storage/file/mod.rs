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
//! open (stale `.tmp` files from an interrupted write are removed then), every write and every
//! retirement updates it under the same lock only after the file system operation succeeded
//! (so a file the file system refused to remove stays indexed and stays counted against the
//! budget), and the directory is owned exclusively by this instance while it is open.
//!
//! Authority law: every store is opened as [`RecordAuthority::Disposable`][disposable] (a cache its
//! owner can rebuild) or [`RecordAuthority::Authoritative`][authoritative] (the only copy of
//! security state, such as the transaction replay store). The authority fixes the two laws below;
//! nothing else differs.
//!
//! [disposable]: crate::storage::file::RecordAuthority::Disposable
//! [authoritative]: crate::storage::file::RecordAuthority::Authoritative
//!
//! Durability law: an authoritative `put` flushes the temporary file to stable storage before
//! renaming it over the record, and flushes the directory after the rename; an authoritative
//! removal flushes the directory after it. A crash therefore leaves each record either whole
//! at its previous value or whole at its new one, never torn, and a completed write or removal
//! is not rolled back. A disposable store skips both flushes: a crash may lose its latest
//! writes or tear a record, which its decode law then discards.
//!
//! Decode law: a disposable store holds only records decodable as `V`. A record the current
//! schema cannot decode (written by an earlier build, or torn) is retired on the read that
//! discovers it and reported absent, so it neither serves stale data nor occupies the budget.
//! The retirement removes exactly the bytes the read observed: a record rewritten between the
//! read and the retirement is the writer's, and stays. An authoritative store never deletes a
//! record it cannot decode: the read that discovers it fails with
//! `Error::StorageRecordUndecodable`, naming the record's file and, when the record's key
//! prefix is intact, its key, and the record stays until its owner or an operator removes it.

use std::io::Write;
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
use crate::storage::UndecodableRecord;

/// What a store's records are to their owner, which fixes how the store writes and reads them
/// (the authority, durability and decode laws of the module documentation).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordAuthority {
    /// A cache its owner can rebuild or do without: writes are not flushed, and a record the
    /// schema cannot decode is retired and reported absent.
    Disposable,
    /// The only copy of security state: writes and removals are flushed to stable storage, and
    /// a record the schema cannot decode is reported and kept.
    Authoritative,
}

impl RecordAuthority {
    /// Whether writes and removals are flushed to stable storage before they return.
    const fn flushes(self) -> bool {
        matches!(self, Self::Authoritative)
    }

    /// Whether a read retires a record it cannot decode instead of reporting it.
    const fn retires_undecodable(self) -> bool {
        matches!(self, Self::Disposable)
    }
}

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

    /// The least recently written file, the next one the budget retires.
    fn oldest(&self) -> Option<String> {
        self.files.oldest().map(str::to_owned)
    }
}

/// One file per key under a byte budget (see the module documentation for its laws).
pub struct FileStorage {
    root: PathBuf,
    capacity: u64,
    authority: RecordAuthority,
    index: RwLock<FileIndex>,
}

impl FileStorage {
    /// Open the disposable store rooted at `path`, creating it if absent, under a budget of
    /// `byte_capacity` serialized bytes.
    ///
    /// Post: the index mirrors the directory (stale `.tmp` files removed) and the budget law
    /// holds, so lowering the configured capacity retires the oldest files at open.
    pub async fn new_with_cap_and_path<P>(byte_capacity: u32, path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        Self::open(byte_capacity, path, RecordAuthority::Disposable)
    }

    /// Open the authoritative store rooted at `path`, creating it if absent, under a budget of
    /// `byte_capacity` serialized bytes: the store of security state, whose writes are flushed
    /// and whose undecodable records are reported and kept.
    ///
    /// Post: as for [`Self::new_with_cap_and_path`].
    pub async fn new_authoritative_with_cap_and_path<P>(
        byte_capacity: u32,
        path: P,
    ) -> Result<Self>
    where
        P: AsRef<std::path::Path>,
    {
        Self::open(byte_capacity, path, RecordAuthority::Authoritative)
    }

    /// Open the store rooted at `path` with `authority`, creating it if absent, under a budget
    /// of `byte_capacity` serialized bytes.
    ///
    /// Post: the index mirrors the directory (stale `.tmp` files removed) and the budget law
    /// holds, so lowering the configured capacity retires the oldest files at open.
    fn open<P>(byte_capacity: u32, path: P, authority: RecordAuthority) -> Result<Self>
    where P: AsRef<std::path::Path> {
        std::fs::create_dir_all(path.as_ref()).map_err(Error::ServiceIOError)?;
        let storage = Self {
            root: path.as_ref().to_path_buf(),
            capacity: u64::from(byte_capacity),
            authority,
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
    /// Post: `Ok(())` implies `index.used_bytes + incoming <= capacity`; on `Err`, every file
    /// the file system refused to remove is still indexed (the index law).
    fn retire_until_fits(&self, index: &mut FileIndex, incoming: u64) -> Result<()> {
        while index.used_bytes.saturating_add(incoming) > self.capacity {
            let Some(name) = index.oldest() else {
                break;
            };
            self.retire_indexed(index, &name)?;
        }
        Ok(())
    }

    /// Remove the record stored as `name` from the directory, then forget it.
    ///
    /// Post: `name` is forgotten iff its file is gone.
    fn retire_indexed(&self, index: &mut FileIndex, name: &str) -> Result<()> {
        remove_file_if_present(&self.root.join(name))?;
        index.forget(name);
        Ok(())
    }

    /// Make the record written to `tmp_path` the stored record `name` of `required` bytes,
    /// retiring other records as the budget demands.
    ///
    /// Post: on `Ok` the index records `name` as the newest file; on `Err` nothing was renamed,
    /// every file still on disk is still indexed, and a previous record under `name` is still
    /// on disk and indexed (re-recorded as the newest file, the one recency the index cannot
    /// restore exactly).
    fn commit_record(
        &self,
        index: &mut FileIndex,
        name: String,
        path: &Path,
        tmp_path: &Path,
        required: u64,
    ) -> Result<()> {
        // The rewritten key does not compete with itself for the budget.
        let previous_len = index.files.get(&name).copied();
        index.forget(&name);
        let renamed = self
            .retire_until_fits(index, required)
            .and_then(|()| std::fs::rename(tmp_path, path).map_err(Error::ServiceIOError));
        match renamed {
            Ok(()) => {
                index.record(name, required);
                Ok(())
            }
            Err(error) => {
                if let Some(previous_len) = previous_len {
                    index.record(name, previous_len);
                }
                Err(error)
            }
        }
    }

    fn read_index(&self) -> Result<RwLockReadGuard<'_, FileIndex>> {
        self.index.read().map_err(|_| Error::LockPoisoned)
    }

    fn write_index(&self) -> Result<RwLockWriteGuard<'_, FileIndex>> {
        self.index.write().map_err(|_| Error::LockPoisoned)
    }

    /// Remove the record stored as `name` and forget it; an authoritative store flushes the
    /// removal (the durability law).
    fn retire(&self, name: &str) -> Result<()> {
        let mut index = self.write_index()?;
        self.retire_indexed(&mut index, name)?;
        self.flush_directory()
    }

    /// Flush the directory's entries (renames and removals) to stable storage iff the store is
    /// authoritative.
    fn flush_directory(&self) -> Result<()> {
        match self.authority.flushes() {
            true => sync_directory(&self.root),
            false => Ok(()),
        }
    }

    /// Decode one record file under the store's authority (the decode law): a disposable store
    /// retires a record the current schema cannot read and reports it absent; an authoritative
    /// store fails with the record named, and keeps it.
    fn decode_record<V>(&self, name: &str, data: &[u8]) -> Result<Option<(String, V)>>
    where V: DeserializeOwned {
        match decode_pair::<V>(name, data) {
            Ok(record) => Ok(Some(record)),
            Err(_) if self.authority.retires_undecodable() => {
                self.retire_observed(name, data)?;
                Ok(None)
            }
            Err(undecodable) => Err(Error::StorageRecordUndecodable(undecodable)),
        }
    }

    /// Retire the record stored as `name` iff its file still holds `observed`, the bytes a read
    /// saw under the read guard. The read released that guard before deciding, so a `put` may
    /// have replaced the record meanwhile; that record is the writer's to keep.
    fn retire_observed(&self, name: &str, observed: &[u8]) -> Result<()> {
        let mut index = self.write_index()?;
        let current = match std::fs::read(self.root.join(name)) {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::ServiceIOError(error)),
        };
        if current == observed {
            self.retire_indexed(&mut index, name)?;
        }
        Ok(())
    }
}

/// Decode `data`, the bytes of the file `name`, as a `(key, value)` record (pure).
///
/// An undecodable record is named by its file and, when its leading key decodes and hashes to
/// `name` (a torn record loses its tail first), by its key as well.
fn decode_pair<V>(name: &str, data: &[u8]) -> std::result::Result<(String, V), UndecodableRecord>
where V: DeserializeOwned {
    rings_codec::deserialize::<(String, V)>(data).map_err(|_| UndecodableRecord {
        name: name.to_owned(),
        key: rings_codec::deserialize_prefix::<String>(data)
            .ok()
            .map(|(key, _)| key)
            .filter(|key| file_name_for(key) == name),
    })
}

/// Write `data` to the fresh file `path`, flushing it to stable storage iff `flush`.
fn write_file(path: &Path, data: &[u8], flush: bool) -> Result<()> {
    let mut file = std::fs::File::create(path).map_err(Error::ServiceIOError)?;
    file.write_all(data).map_err(Error::ServiceIOError)?;
    match flush {
        true => file.sync_all().map_err(Error::ServiceIOError),
        false => Ok(()),
    }
}

/// Flush the entries of directory `root` (the renames and removals made in it) to stable
/// storage.
#[cfg(unix)]
fn sync_directory(root: &Path) -> Result<()> {
    std::fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(Error::ServiceIOError)
}

/// The standard library cannot open a directory for flushing on this platform; the durability
/// of a rename or removal is then the file system's own.
#[cfg(not(unix))]
fn sync_directory(_root: &Path) -> Result<()> {
    Ok(())
}

/// The file name of `key`'s record: the hex SHA-1 digest of the key.
fn file_name_for(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Remove the file `path`, treating an already absent file as removed.
fn remove_file_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::ServiceIOError(error)),
    }
}

#[async_trait]
impl<V> KvStorageInterface<V> for FileStorage
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
        let written = write_file(&tmp_path, &data, self.authority.flushes());
        if written.is_err() {
            remove_file_if_present(&tmp_path)?;
        }
        written?;
        tracing::debug!("Try inserting key: {:?}", key);
        let committed = self.commit_record(&mut index, name, &path, &tmp_path, required);
        if committed.is_err() {
            remove_file_if_present(&tmp_path)?;
        }
        committed?;
        // An error here leaves the new record in place and indexed, its survival of a crash
        // unknown; the caller treats the write as failed.
        self.flush_directory()
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
        while let Some(name) = index.oldest() {
            self.retire_indexed(&mut index, &name)?;
        }
        self.flush_directory()
    }

    async fn count(&self) -> Result<u32> {
        let count = self.read_index()?.files.len();
        u32::try_from(count).map_err(|_| Error::StorageCountOverflow)
    }
}

/// The record name of a directory entry: a 40-character hex file name, or `None` for any other
/// entry.
fn entry_file_name(path: &Path) -> Option<&str> {
    let file_name = path.file_name().and_then(|name| name.to_str())?;
    (file_name.len() == 40 && file_name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .then_some(file_name)
}

impl std::fmt::Debug for FileStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStorage")
            .field("capacity", &self.capacity)
            .field("authority", &self.authority)
            .field("root", &self.root)
            .finish()
    }
}

#[cfg(test)]
mod test_file;
