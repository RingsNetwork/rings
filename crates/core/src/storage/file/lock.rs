//! The exclusive ownership of a store directory (#1039): the precondition of the index law.
//!
//! **Ownership law.** At most one open store holds a store directory. The open takes an
//! exclusive lock on the file [`LOCK_FILE_NAME`] in the directory before it reads or changes
//! anything there, and holds it until the store is dropped; while another holder has it, the
//! open fails with [`Error::StorageDirectoryLocked`], changing nothing. The lock is per store,
//! not per storage root, so two stores under one root (the node layout) open independently,
//! and a second opener of one store, in this process or another, fails at open instead of at
//! its first write.
//!
//! ```text
//! open d:   create d/.lock ; try_lock ─┬─ held     ▶ Err(StorageDirectoryLocked(d))
//!                                      └─ acquired ▶ scan d … (the index law)
//! drop:     unlock d/.lock ; close
//! ```
//!
//! `File::try_lock` is `flock(LOCK_EX | LOCK_NB)` on unix and `LockFileEx` with
//! `LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY` on Windows; each binds the lock to the
//! open file, so two opens within one process exclude each other as two processes do. A file
//! system that supports no such lock fails the open with its I/O error: the store refuses to run
//! without its precondition. The lock file is not a record: it is never indexed, counted, or
//! charged against the budget, and it outlives the store, since removing it while another opener
//! waits on it would let two holders lock two different files.

use std::path::Path;
use std::path::PathBuf;

use crate::error::Error;
use crate::error::Result;

/// The file name of a store directory's lock file.
pub(super) const LOCK_FILE_NAME: &str = ".lock";

/// The exclusive lock of one store directory, released when dropped (the ownership law).
#[derive(Debug)]
pub(super) struct DirectoryLock {
    /// The open lock file the lock is bound to.
    file: std::fs::File,
}

/// Take the exclusive lock of the store directory `root` (the ownership law).
///
/// Pre: `root` exists.
/// Post: `Ok` holds the lock until dropped; `Err(StorageDirectoryLocked(root))` iff another
/// holder has it; any other failure (including a file system without locks) is its I/O error.
pub(super) fn lock_directory(root: &Path) -> Result<DirectoryLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(LOCK_FILE_NAME))
        .map_err(Error::ServiceIOError)?;
    match file.try_lock() {
        Ok(()) => Ok(DirectoryLock { file }),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err(Error::StorageDirectoryLocked(PathBuf::from(root)))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(Error::ServiceIOError(error)),
    }
}

impl Drop for DirectoryLock {
    /// Release the lock; closing the file right after releases it as well, so a failed unlock
    /// is only reported.
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            tracing::warn!("cannot unlock a store directory: {error}");
        }
    }
}
