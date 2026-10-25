//! Group commit on the file store (#916; durable-records, Law Group commit): a batch of records
//! made durable by one barrier and one directory flush, instead of a flush pair per record.
//!
//! ```text
//! put* W (durable):  ∀n ∈ dom W: write n.tmp ; push n.tmp      (data reaches the device)
//!                    barrier                                    (every pushed byte durable)
//!                    ∀n: rename n.tmp → n ; sync dir
//!                    [∃n ∉ P: ∀n ∉ P: mark n ; sync dir]       (the presence law)
//! ```
//!
//! **Law (atomicity per record).** A rename is flushed only after the barrier made its data
//! durable, so at every crash point each name holds its previous record or its new one, never a
//! torn one; a marker is created only after its rename was flushed, so markers name records
//! (`P ⊆ R`). Nothing is promised across names. `test_durability` model-checks the plan over
//! every crash point, with the effects of each name independent.
//!
//! **Platform reading.** On Apple targets `push` is `fsync`, which moves a file's data to the
//! drive without flushing the drive's cache, and the barrier is one `fcntl(F_FULLFSYNC)`, which
//! flushes the whole cache: a batch pays the two (or three, with a new marker) full flushes one
//! `put` pays. Elsewhere `push` is `File::sync_all` (`fsync`), already durable, and the barrier
//! is empty; the directory flush is still paid once per batch.
//!
//! **Budget.** A store that refuses (never evicts) checks the whole batch against its budget
//! before its first write, so a refused batch changes nothing; an evicting store makes room per
//! rename, as a `put` does.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::presence;
use super::remove_file_if_present;
use super::sync_directory;
use super::write_temporary;
use super::FileIndex;
use super::FileStore;
use super::RecordAuthority;
use crate::error::Error;
use crate::error::Result;

/// One file-system step of a batch, as data the store interprets (the durability law).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BatchStep {
    /// Write every record to its temporary file.
    WriteTemporaries,
    /// Move every temporary file's data to the device.
    PushTemporaries,
    /// Make every pushed byte durable.
    Barrier,
    /// Rename every temporary file over its record, making room first (the budget law).
    Renames,
    /// Flush the directory's entries (the renames, and any eviction) to stable storage.
    SyncDirectory,
    /// Mark every record of the batch present that is not, and flush the directory once (the
    /// presence law): only after the renames are durable.
    Witness,
}

impl RecordAuthority {
    /// The plan of a batch (pure; the durability and presence laws): a durable store's plan
    /// pushes, barriers and flushes, and marks iff it witnesses presence; a disposable store's
    /// writes and renames only.
    pub(super) const fn batch_plan(self) -> &'static [BatchStep] {
        match (self.flushes(), self.witnesses()) {
            (true, true) => &[
                BatchStep::WriteTemporaries,
                BatchStep::PushTemporaries,
                BatchStep::Barrier,
                BatchStep::Renames,
                BatchStep::SyncDirectory,
                BatchStep::Witness,
            ],
            (true, false) => &[
                BatchStep::WriteTemporaries,
                BatchStep::PushTemporaries,
                BatchStep::Barrier,
                BatchStep::Renames,
                BatchStep::SyncDirectory,
            ],
            (false, _) => &[BatchStep::WriteTemporaries, BatchStep::Renames],
        }
    }
}

/// One record of a batch: its file name, its encoded bytes, and its record and temporary paths.
struct Staged {
    /// The record's file name.
    name: String,
    /// The encoded record.
    data: Vec<u8>,
    /// The record's path.
    path: PathBuf,
    /// The temporary file the write goes through.
    tmp_path: PathBuf,
}

impl FileStore {
    /// Store the encoded records `batch` (file name, bytes; a later pair of one name supersedes
    /// an earlier one) by running the store's [`RecordAuthority::batch_plan`] (module docs).
    ///
    /// Post: on `Ok` every record is stored and indexed, durable under the store's authority; a
    /// failed step removes every temporary file left, and leaves each record renamed before it
    /// in place and indexed, its survival of a crash unknown: the caller treats the batch as
    /// failed. A refused budget changes nothing.
    pub(super) fn store_batch(&self, batch: Vec<(String, Vec<u8>)>) -> Result<()> {
        let staged = batch
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(name, data)| {
                let path = self.root.join(&name);
                let tmp_path = path.with_extension("tmp");
                Staged {
                    name,
                    data,
                    path,
                    tmp_path,
                }
            })
            .collect::<Vec<_>>();
        if staged.is_empty() {
            return Ok(());
        }
        let mut index = self.write_index()?;
        if !self.authority.evicts() {
            self.admit_batch(&index, &staged)?;
        } else {
            std::fs::create_dir_all(&self.root).map_err(Error::ServiceIOError)?;
        }
        let mut files = Vec::with_capacity(staged.len());
        for step in self.authority.batch_plan() {
            let done = match step {
                BatchStep::WriteTemporaries => staged.iter().try_for_each(|record| {
                    write_temporary(&record.tmp_path, &record.data).map(|file| files.push(file))
                }),
                BatchStep::PushTemporaries => files.iter().try_for_each(push),
                BatchStep::Barrier => files.last().map_or(Ok(()), barrier),
                BatchStep::Renames => staged
                    .iter()
                    .try_for_each(|record| self.rename_staged(&mut index, record)),
                BatchStep::SyncDirectory => sync_directory(&self.root),
                BatchStep::Witness => self.witness_batch(&mut index, &staged),
            };
            if let Err(error) = done {
                // Temporary files live outside the index, so removing them changes nothing.
                for record in &staged {
                    remove_file_if_present(&record.tmp_path)?;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Refuse the whole batch, changing nothing, unless every record of it fits the budget
    /// beside the records it does not replace (the budget law of a store that never evicts).
    fn admit_batch(&self, index: &FileIndex, staged: &[Staged]) -> Result<()> {
        let mut required = 0_u64;
        let mut replaced = 0_u64;
        for record in staged {
            let len = u64::try_from(record.data.len()).map_err(|_| Error::StorageCountOverflow)?;
            required = required.saturating_add(len);
            replaced = replaced.saturating_add(index.files.get(&record.name).copied().unwrap_or(0));
        }
        let kept = index.used_bytes.saturating_sub(replaced);
        if kept.saturating_add(required) > self.capacity {
            return Err(Error::StorageBudgetExhausted {
                used: index.used_bytes,
                required,
                capacity: self.capacity,
            });
        }
        Ok(())
    }

    /// Rename the staged record over its file and index it ([`FileStore::commit_record`]).
    fn rename_staged(&self, index: &mut FileIndex, record: &Staged) -> Result<()> {
        let required = u64::try_from(record.data.len()).map_err(|_| Error::StorageCountOverflow)?;
        self.commit_record(
            index,
            record.name.clone(),
            &record.path,
            &record.tmp_path,
            required,
        )
    }

    /// Mark every staged record present that is not, then flush the directory once, iff any
    /// marker was created (the presence law).
    fn witness_batch(&self, index: &mut FileIndex, staged: &[Staged]) -> Result<()> {
        let unmarked = staged
            .iter()
            .filter(|record| !index.witnessed.contains(&record.name))
            .collect::<Vec<_>>();
        if unmarked.is_empty() {
            return Ok(());
        }
        for record in unmarked {
            presence::create_marker(&self.root, &record.name, self.authority.flushes())
                .map_err(Error::ServiceIOError)?;
            index.witnessed.insert(record.name.clone());
        }
        sync_directory(&self.root)
    }
}

/// Move `file`'s data to the device without flushing the drive's cache: `fsync`, which on Apple
/// targets stops short of the cache (module docs).
#[cfg(target_vendor = "apple")]
fn push(file: &std::fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: `file` is open for the whole call, so its descriptor is valid, and `fsync` reads
    // nothing but the descriptor.
    match unsafe { libc::fsync(file.as_raw_fd()) } {
        0 => Ok(()),
        _ => Err(Error::ServiceIOError(std::io::Error::last_os_error())),
    }
}

/// Make `file`'s data durable: `File::sync_all`, which is `fsync` here and needs no barrier
/// (module docs).
#[cfg(not(target_vendor = "apple"))]
fn push(file: &std::fs::File) -> Result<()> {
    file.sync_all().map_err(Error::ServiceIOError)
}

/// Make every pushed byte durable: `File::sync_all` on any one file, which on Apple targets is
/// `fcntl(F_FULLFSYNC)` and flushes the drive's whole cache (module docs).
#[cfg(target_vendor = "apple")]
fn barrier(file: &std::fs::File) -> Result<()> {
    file.sync_all().map_err(Error::ServiceIOError)
}

/// Nothing: every pushed file is already durable on this target (module docs).
#[cfg(not(target_vendor = "apple"))]
fn barrier(_file: &std::fs::File) -> Result<()> {
    Ok(())
}
