#![deny(missing_docs)]

//! Persistent native key-value storage: one file per key under a byte budget.
//!
//! Authority law: every store is opened as [`RecordAuthority::Disposable`][disposable] (a cache
//! its owner can rebuild) or [`RecordAuthority::Authoritative`][authoritative] (the only copy of
//! security state, such as the transaction replay store). The authority fixes the budget,
//! durability, open and decode laws below; nothing else differs.
//!
//! [disposable]: crate::storage::file::RecordAuthority::Disposable
//! [authoritative]: crate::storage::file::RecordAuthority::Authoritative
//!
//! Budget law: the bytes of every stored file sum to at most `capacity`, and a value larger
//! than the whole budget is rejected with `Error::StorageValueExceedsCapacity`, changing
//! nothing. A disposable `put` whose value would exceed the budget first retires the least
//! recently written *other* keys until the value fits, and a disposable open retires the oldest
//! files until a lowered capacity holds. An authoritative store retires nothing: the `put` and
//! the open fail with `Error::StorageBudgetExhausted` instead, changing nothing, since evicting a
//! record of security state would silently roll it back.
//!
//! Index law: the in-memory index mirrors the directory. It is rebuilt from the directory on
//! open (stale `.tmp` files from an interrupted write are removed then), every write and every
//! retirement updates it under the same lock only after the file system operation succeeded
//! (so a file the file system refused to remove stays indexed and stays counted against the
//! budget), and the directory is owned exclusively by this instance while it is open. A record
//! file whose metadata cannot be read fails a disposable open; an authoritative open indexes it
//! at zero bytes so that a scan reports it rather than hiding it. A directory listing that fails
//! part-way skips the unlisted entries of a disposable store, and fails an authoritative open as
//! a whole, since an entry it cannot list it cannot name.
//!
//! Durability law: every write and removal runs a fixed plan of file-system steps, data interpreted
//! by the store (`PutStep`, `RemoveStep`). An authoritative `put` writes the temporary file,
//! flushes it, renames it over the record, and flushes the directory; an authoritative removal
//! removes the record and flushes the directory; an authoritative open flushes the root and its
//! ancestors, so the store's own directory entry survives. On unix a crash therefore leaves each
//! record either whole at its previous value or whole at its new one, never torn, and a completed
//! write or removal is not rolled back. `test_durability` model-checks the plans over every crash
//! point; each step's effect is its one-line arm in the interpreter. The flushes are
//! `File::sync_all`, which the standard library maps to `fcntl(F_FULLFSYNC)` on Apple targets
//! (flushing the drive cache as well) and to `fsync` elsewhere; the law rests on that mapping. On
//! other targets a directory cannot be flushed, so the durability of a rename or a removal is the
//! file system's own. A disposable store's plans skip every flush: a crash may lose its latest
//! writes or tear a record, which its decode law then discards.
//!
//! Decode law: a record is the file's only if it decodes whole as `(key, V)` and its key hashes
//! to the file name; the store writes nothing else. Any other record is undecodable for the name
//! it is found under. A disposable store retires such a record on the read that discovers it
//! and reports it absent, so it neither serves stale data nor occupies the budget; the
//! retirement removes exactly the bytes the read observed, so a record rewritten between the
//! read and the retirement is the writer's, and stays. An authoritative store never deletes it:
//! the read that discovers it fails with `Error::StorageRecordUndecodable`, naming the file, and
//! the record stays until its owner or an operator removes it. A
//! [`scan`](crate::storage::KvStorageScan::scan) deletes nothing under either authority, and
//! reports every record file as filed or undecodable by its name, a file it cannot read (any
//! error but its absence) included.
//!
//! Execution law: all file system work of an operation, flushes included, runs on the blocking
//! thread pool of the tokio runtime, so a slow flush never stalls an asynchronous worker.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;

use super::write_ordered::WriteOrderedMap;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;
use crate::storage::ScannedRecord;
use crate::storage::UndecodableRecord;

/// What a store's records are to their owner, which fixes how the store writes and reads them
/// (the authority law of the module documentation).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordAuthority {
    /// A cache its owner can rebuild or do without: writes are not flushed, the budget evicts
    /// the oldest records, and a record the schema cannot decode is retired and reported
    /// absent.
    Disposable,
    /// The only copy of security state: writes and removals are flushed to stable storage,
    /// nothing is evicted, and a record the schema cannot decode is reported and kept.
    Authoritative,
}

impl RecordAuthority {
    /// Whether the open flushes the store's directory entry (the durability law).
    const fn flushes(self) -> bool {
        matches!(self, Self::Authoritative)
    }

    /// Whether a read retires a record it cannot decode instead of reporting it.
    const fn retires_undecodable(self) -> bool {
        matches!(self, Self::Disposable)
    }

    /// Whether the budget retires the oldest records to make room (the budget law), and a write
    /// recreates the root directory should it be missing.
    const fn evicts(self) -> bool {
        matches!(self, Self::Disposable)
    }

    /// Whether every listed record file must be indexed, so that a listing that fails fails the
    /// open and a file whose metadata fails is indexed anyway (the index law).
    const fn indexes_every_entry(self) -> bool {
        matches!(self, Self::Authoritative)
    }

    /// The plan of a `put` (pure; the durability law).
    const fn put_plan(self) -> &'static [PutStep] {
        match self {
            Self::Authoritative => &[
                PutStep::WriteTemporary,
                PutStep::SyncTemporary,
                PutStep::Rename,
                PutStep::SyncDirectory,
            ],
            Self::Disposable => &[PutStep::WriteTemporary, PutStep::Rename],
        }
    }

    /// The plan of a removal (pure; the durability law).
    const fn remove_plan(self) -> &'static [RemoveStep] {
        match self {
            Self::Authoritative => &[RemoveStep::Remove, RemoveStep::SyncDirectory],
            Self::Disposable => &[RemoveStep::Remove],
        }
    }
}

/// One file-system step of a `put`, as data the store interprets (the durability law).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PutStep {
    /// Write the record to its temporary file.
    WriteTemporary,
    /// Flush the temporary file's contents to stable storage.
    SyncTemporary,
    /// Rename the temporary file over the record, making room first (the budget law).
    Rename,
    /// Flush the directory's entries (the rename, and any eviction) to stable storage.
    SyncDirectory,
}

/// One file-system step of a removal, as data the store interprets (the durability law).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoveStep {
    /// Remove the records and forget them.
    Remove,
    /// Flush the directory's entries (the removals) to stable storage.
    SyncDirectory,
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
///
/// A shared handle on the synchronous store, whose file system work every operation runs on
/// the blocking pool (the execution law).
pub struct FileStorage {
    store: Arc<FileStore>,
}

/// The synchronous store behind a [`FileStorage`]: every method blocks on the file system.
struct FileStore {
    root: PathBuf,
    capacity: u64,
    authority: RecordAuthority,
    index: RwLock<FileIndex>,
}

/// One record file as a read found it: its bytes, or the error that kept them from the reader.
type ReadRecord = (String, std::io::Result<Vec<u8>>);

impl FileStorage {
    /// Open the disposable store rooted at `path`, creating it if absent, under a budget of
    /// `byte_capacity` serialized bytes.
    ///
    /// Post: the index mirrors the directory (stale `.tmp` files removed) and the budget law
    /// holds, so lowering the configured capacity retires the oldest files at open.
    pub async fn new_with_cap_and_path<P>(byte_capacity: u32, path: P) -> Result<Self>
    where P: AsRef<std::path::Path> {
        Self::new_with_cap_path_and_authority(byte_capacity, path, RecordAuthority::Disposable)
            .await
    }

    /// Open the store rooted at `path` with `authority`, creating it if absent, under a budget
    /// of `byte_capacity` serialized bytes. [`RecordAuthority::Authoritative`] opens a store of
    /// security state, whose writes are flushed, which evicts nothing, and whose undecodable
    /// records are reported and kept.
    ///
    /// Post: the index mirrors the directory (stale `.tmp` files removed) and the budget law
    /// holds: a disposable open retires the oldest files down to the capacity, and an
    /// authoritative open over the budget fails. A successful authoritative open has flushed
    /// the store's directory entries within the durability law's bound.
    pub async fn new_with_cap_path_and_authority<P>(
        byte_capacity: u32,
        path: P,
        authority: RecordAuthority,
    ) -> Result<Self>
    where
        P: AsRef<std::path::Path>,
    {
        let root = path.as_ref().to_path_buf();
        let store = blocking(move || FileStore::open(byte_capacity, root, authority)).await?;
        Ok(Self {
            store: Arc::new(store),
        })
    }

    /// Run `operation` on the store from the blocking pool (the execution law).
    async fn on_store<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&FileStore) -> Result<T> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        blocking(move || operation(store.as_ref())).await
    }

    /// Settle a record that is not the whole record of its file's key, whose file held `data`,
    /// under the decode law: a disposable store retires it (iff the file still holds `data`)
    /// and reports it absent with `Ok`; an authoritative store fails with it named, and keeps
    /// it.
    async fn settle_undecodable(
        &self,
        undecodable: UndecodableRecord,
        data: Vec<u8>,
    ) -> Result<()> {
        if !self.store.authority.retires_undecodable() {
            return Err(Error::StorageRecordUndecodable(undecodable));
        }
        let name = undecodable.name;
        self.on_store(move |store| store.retire_observed(&name, &data))
            .await
    }

    /// The pair `record` holds if it is filed under its own key; an undecodable record, read
    /// from bytes `data`, is settled ([`Self::settle_undecodable`]) and yields `None`.
    async fn filed_pair<V>(
        &self,
        data: Vec<u8>,
        record: ScannedRecord<V>,
    ) -> Result<Option<(String, V)>> {
        match record {
            ScannedRecord::Filed { key, value } => Ok(Some((key, value))),
            ScannedRecord::Undecodable(undecodable) => {
                self.settle_undecodable(undecodable, data).await?;
                Ok(None)
            }
        }
    }
}

/// Run the blocking `operation` on the runtime's blocking pool (the execution law).
///
/// Post: `Err(StorageWorkUnscheduled)` when no runtime is current (nothing ran) or the work
/// panicked; dropping the returned future abandons only the wait, never the work.
async fn blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    rings_runtime::run_blocking(operation)
        .await
        .map_err(Error::StorageWorkUnscheduled)?
}

impl FileStore {
    /// Open the store rooted at `root` with `authority`, creating it if absent, under a budget
    /// of `byte_capacity` serialized bytes.
    ///
    /// Post: the index mirrors the directory (stale `.tmp` files removed) and the budget law
    /// holds: a disposable open retires the oldest files down to the capacity, and an
    /// authoritative open over it fails.
    fn open(byte_capacity: u32, root: PathBuf, authority: RecordAuthority) -> Result<Self> {
        create_directory(&root, authority.flushes())?;
        let store = Self {
            root,
            capacity: u64::from(byte_capacity),
            authority,
            index: RwLock::new(FileIndex::default()),
        };
        let mut index = store.write_index()?;
        *index = store.scan_directory()?;
        store.make_room(&mut index, 0)?;
        drop(index);
        Ok(store)
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
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                // An unlisted record cannot be named, so an authoritative store fails whole.
                Err(error) if self.authority.indexes_every_entry() => {
                    return Err(Error::ServiceIOError(error));
                }
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "tmp") {
                remove_file_if_present(&path)?;
                continue;
            }
            let Some(name) = entry_file_name(&path) else {
                continue;
            };
            let (modified, len) = match std::fs::metadata(&path) {
                Ok(metadata) => (
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    metadata.len(),
                ),
                // Indexed so that a scan reports it (the index law).
                Err(_) if self.authority.indexes_every_entry() => (SystemTime::UNIX_EPOCH, 0),
                Err(error) => return Err(Error::ServiceIOError(error)),
            };
            files.push((modified, name.to_owned(), len));
        }
        files.sort();
        let mut index = FileIndex::default();
        for (_, name, len) in files {
            index.record(name, len);
        }
        Ok(index)
    }

    /// Make room for `incoming` more bytes under the budget law: a disposable store retires its
    /// least recently written files until they fit; an authoritative store fails instead.
    ///
    /// Pre: `incoming <= capacity`.
    /// Post: `Ok(())` implies `index.used_bytes + incoming <= capacity`; on `Err`, every file
    /// the file system refused to remove is still indexed (the index law), and an
    /// authoritative store removed nothing.
    fn make_room(&self, index: &mut FileIndex, incoming: u64) -> Result<()> {
        while index.used_bytes.saturating_add(incoming) > self.capacity {
            if !self.authority.evicts() {
                return Err(Error::StorageBudgetExhausted {
                    used: index.used_bytes,
                    required: incoming,
                    capacity: self.capacity,
                });
            }
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
    /// making room as the budget law demands.
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
            .make_room(index, required)
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

    /// Store `data`, the encoded record of `key`, as the file `name` by running the store's
    /// [`RecordAuthority::put_plan`] (the durability law).
    ///
    /// Post: as [`Self::commit_record`]; a failed step removes the temporary file, and an error
    /// from the final directory flush leaves the new record in place and indexed, its survival
    /// of a crash unknown, and the caller treats the write as failed.
    fn store_record(&self, name: String, data: &[u8]) -> Result<()> {
        let required = u64::try_from(data.len()).map_err(|_| Error::StorageCountOverflow)?;
        let path = self.root.join(&name);
        let tmp_path = path.with_extension("tmp");
        let mut index = self.write_index()?;
        // The root is the store's own (the index law); only a disposable store recreates it.
        if self.authority.evicts() {
            std::fs::create_dir_all(&self.root).map_err(Error::ServiceIOError)?;
        }
        let mut temporary = None;
        for step in self.authority.put_plan() {
            let done = match step {
                PutStep::WriteTemporary => {
                    write_temporary(&tmp_path, data).map(|file| temporary = Some(file))
                }
                PutStep::SyncTemporary => temporary.as_ref().map_or(Ok(()), |file| {
                    file.sync_all().map_err(Error::ServiceIOError)
                }),
                PutStep::Rename => {
                    self.commit_record(&mut index, name.clone(), &path, &tmp_path, required)
                }
                PutStep::SyncDirectory => sync_directory(&self.root),
            };
            if let Err(error) = done {
                // The temporary file lives outside the index, so removing it changes nothing.
                remove_file_if_present(&tmp_path)?;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Remove the records `names` and forget them by running the store's
    /// [`RecordAuthority::remove_plan`] (the durability law).
    ///
    /// Post: a failed removal leaves the record it could not remove indexed (the index law); an
    /// error from the final directory flush leaves every removal made and forgotten, its survival
    /// of a crash unknown, and the caller treats the removal as failed.
    fn remove_records(&self, index: &mut FileIndex, names: &[String]) -> Result<()> {
        for step in self.authority.remove_plan() {
            match step {
                RemoveStep::Remove => {
                    for name in names {
                        self.retire_indexed(index, name)?;
                    }
                }
                RemoveStep::SyncDirectory => sync_directory(&self.root)?,
            }
        }
        Ok(())
    }

    /// Acquire the index for reading.
    fn read_index(&self) -> Result<RwLockReadGuard<'_, FileIndex>> {
        self.index.read().map_err(|_| Error::LockPoisoned)
    }

    /// Acquire the index for writing.
    fn write_index(&self) -> Result<RwLockWriteGuard<'_, FileIndex>> {
        self.index.write().map_err(|_| Error::LockPoisoned)
    }

    /// The bytes of the record file `name`, read under the read guard; `None` if it is absent.
    fn read_record(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let _guard = self.read_index()?;
        read_file_if_present(&self.root.join(name)).map_err(Error::ServiceIOError)
    }

    /// Read every indexed record file under the read guard, each to its bytes or to the error
    /// that kept them; a file removed since it was indexed is skipped. Only the index lock can
    /// fail the read as a whole.
    fn read_records(&self) -> Result<Vec<ReadRecord>> {
        let index = self.read_index()?;
        Ok(index
            .files
            .iter()
            .filter_map(|(name, _)| {
                let read = read_file_if_present(&self.root.join(name)).transpose()?;
                Some((name.to_owned(), read))
            })
            .collect())
    }

    /// Remove the record stored as `name` and forget it (the remove plan).
    fn retire(&self, name: &str) -> Result<()> {
        let mut index = self.write_index()?;
        self.remove_records(&mut index, &[name.to_owned()])
    }

    /// Remove every record (the remove plan).
    fn clear(&self) -> Result<()> {
        let mut index = self.write_index()?;
        let names = index
            .files
            .iter()
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>();
        self.remove_records(&mut index, &names)
    }

    /// Retire the record stored as `name` iff its file still holds `observed`, the bytes a read
    /// saw under the read guard. The read released that guard before deciding, so a `put` may
    /// have replaced the record meanwhile; that record is the writer's to keep.
    fn retire_observed(&self, name: &str, observed: &[u8]) -> Result<()> {
        let mut index = self.write_index()?;
        let current = read_file_if_present(&self.root.join(name)).map_err(Error::ServiceIOError)?;
        if current.as_deref() == Some(observed) {
            self.retire_indexed(&mut index, name)?;
        }
        Ok(())
    }
}

/// Scan `data`, the bytes of the file `name` (pure): the whole record filed there under its own
/// key, or an undecodable record for `name` (anything else: torn, corrupt, or another key's).
fn scan_bytes<V>(name: String, data: &[u8]) -> ScannedRecord<V>
where V: DeserializeOwned {
    match rings_codec::deserialize::<(String, V)>(data) {
        Ok((key, value)) if file_name_for(&key) == name => ScannedRecord::Filed { key, value },
        _ => ScannedRecord::Undecodable(UndecodableRecord { name }),
    }
}

/// Scan one record file (pure): [`scan_bytes`] of what it holds, or an undecodable record named
/// by its file when it cannot be read.
fn scan_record<V>((name, read): ReadRecord) -> ScannedRecord<V>
where V: DeserializeOwned {
    match read {
        Ok(data) => scan_bytes(name, &data),
        Err(_) => ScannedRecord::Undecodable(UndecodableRecord { name }),
    }
}

/// Create the directory `root` and its missing ancestors; iff `flush`, flush `root` and its
/// ancestors, deepest first, so that the store's directory entries survive a crash, whoever
/// created them (another store may have created a shared parent without flushing it). The walk
/// stops at the first ancestor the process may not open (`PermissionDenied`), one it did not
/// create; any other failure fails the open.
fn create_directory(root: &Path, flush: bool) -> Result<()> {
    std::fs::create_dir_all(root).map_err(Error::ServiceIOError)?;
    if !flush {
        return Ok(());
    }
    for directory in root.ancestors() {
        // A relative path's last ancestor is empty: the working directory.
        let directory = match directory.as_os_str().is_empty() {
            true => Path::new("."),
            false => directory,
        };
        match sync_directory_io(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => break,
            Err(error) => return Err(Error::ServiceIOError(error)),
        }
    }
    Ok(())
}

/// Write `data` to the fresh file `path` and return it open, for [`PutStep::SyncTemporary`] to
/// flush; `File::sync_all` is `fcntl(F_FULLFSYNC)` on Apple targets and `fsync` on other unix
/// targets (a precondition of the durability law).
fn write_temporary(path: &Path, data: &[u8]) -> Result<std::fs::File> {
    let mut file = std::fs::File::create(path).map_err(Error::ServiceIOError)?;
    file.write_all(data).map_err(Error::ServiceIOError)?;
    Ok(file)
}

/// Flush the entries of directory `root` (the renames and removals made in it) to stable
/// storage.
///
/// `File::sync_all` on the directory is `fcntl(F_FULLFSYNC)` on Apple targets and `fsync` on
/// other unix targets (a precondition of the durability law).
fn sync_directory(root: &Path) -> Result<()> {
    sync_directory_io(root).map_err(Error::ServiceIOError)
}

/// Open the directory `root` and flush its entries, reporting the raw I/O error.
#[cfg(unix)]
fn sync_directory_io(root: &Path) -> std::io::Result<()> {
    std::fs::File::open(root).and_then(|directory| directory.sync_all())
}

/// The standard library cannot open a directory for flushing on this platform; the durability
/// of a rename or removal is then the file system's own (the durability law holds on unix
/// only).
#[cfg(not(unix))]
fn sync_directory_io(_root: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The file name of `key`'s record: the hex SHA-1 digest of the key.
fn file_name_for(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Read the file `path`, treating an absent file as `None`.
fn read_file_if_present(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
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
where V: Serialize + DeserializeOwned + Send + Sync
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        let name = file_name_for(key);
        let read_name = name.clone();
        let Some(data) = self
            .on_store(move |store| store.read_record(&read_name))
            .await?
        else {
            return Ok(None);
        };
        let record = scan_bytes::<V>(name, &data);
        // `scan_bytes` already ties a filed key to the file name, so this differs from
        // `Some(value)` only on a SHA-1 collision: a collision guard, not the name check.
        Ok(self
            .filed_pair(data, record)
            .await?
            .and_then(|(stored_key, value)| (stored_key == key).then_some(value)))
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        let data = rings_codec::serialize(&(key, value)).map_err(Error::CodecSerialize)?;
        let required = u64::try_from(data.len()).map_err(|_| Error::StorageCountOverflow)?;
        if required > self.store.capacity {
            return Err(Error::StorageValueExceedsCapacity {
                required,
                capacity: self.store.capacity,
            });
        }
        tracing::debug!("Try inserting key: {:?}", key);
        let name = file_name_for(key);
        self.on_store(move |store| store.store_record(name, &data))
            .await
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let records = self.on_store(FileStore::read_records).await?;
        let mut decoded = Vec::with_capacity(records.len());
        for (name, read) in records {
            let data = read.map_err(Error::ServiceIOError)?;
            let record = scan_bytes::<V>(name, &data);
            decoded.extend(self.filed_pair(data, record).await?);
        }
        Ok(decoded)
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let name = file_name_for(key);
        self.on_store(move |store| store.retire(&name)).await
    }

    async fn clear(&self) -> Result<()> {
        self.on_store(FileStore::clear).await
    }

    async fn count(&self) -> Result<u32> {
        let count = self
            .on_store(|store| Ok(store.read_index()?.files.len()))
            .await?;
        u32::try_from(count).map_err(|_| Error::StorageCountOverflow)
    }
}

#[async_trait]
impl<V> KvStorageScan<V> for FileStorage
where V: Serialize + DeserializeOwned + Send + Sync
{
    /// Every record file, as filed or undecodable, by its file name; a file that cannot be read
    /// is reported too, so one bad file never fails the whole scan. Unlike `get_all`, a scan
    /// never retires, whatever the store's authority.
    async fn scan(&self) -> Result<Vec<ScannedRecord<V>>> {
        Ok(self
            .on_store(FileStore::read_records)
            .await?
            .into_iter()
            .map(scan_record)
            .collect())
    }

    /// The file name of `key`'s record, the hex SHA-1 digest of the key, under which a scan
    /// reports that record when it cannot read or decode it.
    fn record_name(&self, key: &str) -> String {
        file_name_for(key)
    }
}

/// The record name of a directory entry: a 40-character lower-case hex file name (the image of
/// `file_name_for`), or `None` for any other entry.
fn entry_file_name(path: &Path) -> Option<&str> {
    let file_name = path.file_name().and_then(|name| name.to_str())?;
    // The image of `file_name_for`: lower-case hex only.
    let lower_hex = |byte: &u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte);
    (file_name.len() == 40 && file_name.as_bytes().iter().all(lower_hex)).then_some(file_name)
}

impl std::fmt::Debug for FileStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStorage")
            .field("capacity", &self.store.capacity)
            .field("authority", &self.store.authority)
            .field("root", &self.store.root)
            .finish()
    }
}

#[cfg(test)]
mod test_durability;
#[cfg(test)]
mod test_file;
#[cfg(test)]
pub(crate) mod test_root;
