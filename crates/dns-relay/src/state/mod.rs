//! The relay's state files: the effects shared by everything the relay keeps across restarts.
//!
//! - The address key is raw bytes, created once, readable by its owner alone.
//! - The address book and the memory are [`Journal`]s: a persistent map kept as an append-only
//!   log of puts and removes, one JSON record per line, compacted into a snapshot once the log
//!   holds more than twice the live entries. A change costs one appended line, whatever the
//!   size of the map.
//!
//! Both journals hold caches whose loss degrades gracefully (a client resolves a name again, an
//! owner is looked up again), so a record is written and flushed before its change is committed
//! in memory, and replay truncates a torn trailing record at EOF while rejecting corrupt interior
//! records: a crash mid-append costs at most the interrupted tail. A compaction's snapshot is
//! synced before it replaces the log, so a crash never costs the map.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use rand::rngs::OsRng;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::address::AddressKey;
use crate::address::ADDRESS_KEY_LEN;

/// Records a journal holds beyond its live entries before it is compacted, so a small map is not
/// compacted on every change.
const COMPACTION_SLACK: usize = 64;

/// Why the relay's state cannot be read or written.
#[derive(Debug, Error)]
pub enum StateError {
    /// Reading or writing a state file failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
    /// The key file does not hold exactly [`ADDRESS_KEY_LEN`] bytes.
    #[error("{0}: an address key is {ADDRESS_KEY_LEN} bytes")]
    KeyLength(PathBuf),
    /// A journal record does not encode.
    #[error("{path}: {source}")]
    Encode {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: serde_json::Error,
    },
    /// An interior journal record does not decode.
    #[error("{path}:{line}: {source}")]
    Decode {
        /// The file.
        path: PathBuf,
        /// The 1-based line number of the corrupt record.
        line: usize,
        /// The failure.
        source: serde_json::Error,
    },
}

impl StateError {
    /// The I/O failure `source` on `path`.
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Now, in milliseconds since the Unix epoch; 0 before it. The relay's one clock read.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Create the state directory `dir`, and its parents, if missing.
pub(crate) async fn ensure_dir(dir: &Path) -> Result<(), StateError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|source| StateError::io(dir, source))
}

/// The address key stored at `path`, or a new random key, stored there readable by its owner
/// alone.
pub(crate) async fn load_or_create_key(path: &Path) -> Result<AddressKey, StateError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            let key = <[u8; ADDRESS_KEY_LEN]>::try_from(bytes.as_slice())
                .map_err(|_| StateError::KeyLength(path.to_path_buf()))?;
            Ok(AddressKey::from_bytes(Zeroizing::new(key)))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut bytes = Zeroizing::new([0u8; ADDRESS_KEY_LEN]);
            OsRng.fill_bytes(bytes.as_mut_slice());
            write_private(path, bytes.as_slice()).await?;
            Ok(AddressKey::from_bytes(bytes))
        }
        Err(error) => Err(StateError::io(path, error)),
    }
}

/// Write `bytes` to a new file at `path` that only its owner may read.
async fn write_private(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|source| StateError::io(path, source))?;
    file.write_all(bytes)
        .await
        .map_err(|source| StateError::io(path, source))?;
    file.sync_all()
        .await
        .map_err(|source| StateError::io(path, source))
}

/// One record of a journal.
#[derive(Debug, Deserialize, Serialize)]
enum Record<K, V> {
    /// `key` maps to `value`.
    Put(K, V),
    /// `key` maps to nothing.
    Remove(K),
}

/// Truncate `path` to `len` bytes and sync the change.
async fn truncate_file(path: &Path, len: u64) -> Result<(), std::io::Error> {
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.set_len(len).await?;
    file.sync_all().await
}

/// A batch of encoded journal records to append in one write.
#[derive(Default)]
pub(crate) struct JournalBatch {
    text: String,
    records: usize,
}

/// A persistent map kept as an append-only log (see the module doc).
///
/// ```text
///   replay(log) = fold apply ∅ log        apply(m, Put k v) = m[k ↦ v],  apply(m, Remove k) = m ∖ k
///   Law: replay(log ++ [r]) = apply(replay(log), r);  replay(compact(m)) = m
/// ```
pub(crate) struct Journal {
    path: PathBuf,
    records: usize,
    pending_truncate: Option<u64>,
}

impl Journal {
    /// Open the journal at `path` and replay it; a missing journal is an empty map. A torn
    /// trailing record at EOF is warned on and truncated (or deferred until the next write when
    /// the file is read-only), while a corrupt interior record returns [`StateError::Decode`].
    pub(crate) async fn open<K, V>(path: &Path) -> Result<(Self, BTreeMap<K, V>), StateError>
    where
        K: Ord + DeserializeOwned,
        V: DeserializeOwned,
    {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(StateError::io(path, error)),
        };
        let mut map = BTreeMap::new();
        let mut records = 0usize;
        let mut valid_len = 0usize;
        let mut line_no = 0usize;
        let mut torn_tail = false;
        let mut lines = bytes.split_inclusive(|byte| *byte == b'\n').peekable();
        while let Some(raw_line) = lines.next() {
            line_no = line_no.saturating_add(1);
            let is_last = lines.peek().is_none();
            let Some(line) = raw_line.strip_suffix(b"\n") else {
                tracing::warn!(
                    "{}:{line_no}: truncating unterminated trailing journal record",
                    path.display()
                );
                torn_tail = true;
                break;
            };
            match serde_json::from_slice::<Record<K, V>>(line) {
                Ok(Record::Put(key, value)) => {
                    map.insert(key, value);
                }
                Ok(Record::Remove(key)) => {
                    map.remove(&key);
                }
                Err(source) if is_last => {
                    tracing::warn!(
                        "{}:{line_no}: truncating torn trailing journal record: {source}",
                        path.display()
                    );
                    torn_tail = true;
                    break;
                }
                Err(source) => {
                    return Err(StateError::Decode {
                        path: path.to_path_buf(),
                        line: line_no,
                        source,
                    });
                }
            }
            records = records.saturating_add(1);
            valid_len = valid_len.saturating_add(raw_line.len());
        }
        let pending_truncate = if torn_tail {
            let valid_len_u64 = u64::try_from(valid_len).unwrap_or(u64::MAX);
            match truncate_file(path, valid_len_u64).await {
                Ok(()) => None,
                Err(error) => {
                    tracing::warn!(
                        "{}: could not truncate torn tail immediately, deferring until next write: {error}",
                        path.display()
                    );
                    Some(valid_len_u64)
                }
            }
        } else {
            None
        };
        let journal = Self {
            path: path.to_path_buf(),
            records,
            pending_truncate,
        };
        Ok((journal, map))
    }

    /// An empty batch of records for [`Self::append_batch`].
    pub(crate) fn batch(&self) -> JournalBatch {
        JournalBatch::default()
    }

    /// Encode into `batch` the record that `key` maps to `value`.
    pub(crate) fn batch_put<K, V>(
        &self,
        batch: &mut JournalBatch,
        key: &K,
        value: &V,
    ) -> Result<(), StateError>
    where
        K: Serialize,
        V: Serialize,
    {
        batch.text.push_str(&self.encode(&Record::Put(key, value))?);
        batch.records = batch.records.saturating_add(1);
        Ok(())
    }

    /// Encode into `batch` the record that `key` maps to nothing.
    pub(crate) fn batch_remove<K>(
        &self,
        batch: &mut JournalBatch,
        key: &K,
    ) -> Result<(), StateError>
    where
        K: Serialize,
    {
        batch
            .text
            .push_str(&self.encode(&Record::<&K, ()>::Remove(key))?);
        batch.records = batch.records.saturating_add(1);
        Ok(())
    }

    /// Append the record that `key` maps to `value`.
    #[cfg(test)]
    pub(crate) async fn put<K, V>(&mut self, key: &K, value: &V) -> Result<(), StateError>
    where
        K: Serialize,
        V: Serialize,
    {
        let mut batch = self.batch();
        self.batch_put(&mut batch, key, value)?;
        self.append_batch(batch).await?;
        self.confirm_append();
        Ok(())
    }

    /// Append the record that `key` maps to nothing.
    #[cfg(test)]
    pub(crate) async fn remove<K>(&mut self, key: &K) -> Result<(), StateError>
    where K: Serialize {
        let mut batch = self.batch();
        self.batch_remove(&mut batch, key)?;
        self.append_batch(batch).await?;
        self.confirm_append();
        Ok(())
    }

    /// Confirm that the caller has committed the in-memory state corresponding to the last
    /// [`Self::append_batch`], disarming any pending rollback truncation.
    pub(crate) fn confirm_append(&mut self) {
        self.pending_truncate = None;
    }

    /// Truncate any unconfirmed or torn tail left by an interrupted or cancelled append before
    /// inspecting or mutating the journal further.
    pub(crate) async fn settle_pending_truncate(&mut self) -> Result<(), StateError> {
        if let Some(valid_len) = self.pending_truncate {
            match truncate_file(&self.path, valid_len).await {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(source) => return Err(StateError::io(&self.path, source)),
            }
            self.pending_truncate = None;
        }
        Ok(())
    }

    /// Whether a map of `live` entries should be compacted: its log holds more than twice them.
    pub(crate) fn needs_compaction(&self, live: usize) -> bool {
        self.records > live.saturating_mul(2).saturating_add(COMPACTION_SLACK)
    }

    /// Replace the log with one put per entry of `entries`, atomically: written beside, then
    /// renamed over.
    pub(crate) async fn compact<K, V>(
        &mut self,
        entries: impl IntoIterator<Item = (K, V)>,
    ) -> Result<(), StateError>
    where
        K: Serialize,
        V: Serialize,
    {
        self.settle_pending_truncate().await?;
        let mut text = String::new();
        let mut records = 0usize;
        for (key, value) in entries {
            text.push_str(&self.encode(&Record::Put(key, value))?);
            records = records.saturating_add(1);
        }
        let staging = self.path.with_extension("tmp");
        let io = |source| StateError::io(&staging, source);
        let mut file = tokio::fs::File::create(&staging).await.map_err(io)?;
        file.write_all(text.as_bytes()).await.map_err(io)?;
        file.sync_all().await.map_err(io)?;
        drop(file);
        tokio::fs::rename(&staging, &self.path)
            .await
            .map_err(|source| StateError::io(&self.path, source))?;
        self.records = records;
        self.pending_truncate = None;
        Ok(())
    }

    /// Append all records in `batch` in one write and flush; roll back any partial or unconfirmed
    /// write on failure or cancellation so a later operation never leaves the journal ahead of
    /// in-memory state or writes behind a torn tail.
    pub(crate) async fn append_batch(&mut self, batch: JournalBatch) -> Result<(), StateError> {
        self.settle_pending_truncate().await?;
        if batch.records == 0 {
            return Ok(());
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|source| StateError::io(&self.path, source))?;
        let len_before = file
            .metadata()
            .await
            .map_err(|source| StateError::io(&self.path, source))?
            .len();
        self.pending_truncate = Some(len_before);
        // A Tokio file writes in the background: the flush waits until the batch is written, so
        // no later write or compaction overtakes it.
        let write_result = async {
            file.write_all(batch.text.as_bytes()).await?;
            file.flush().await
        }
        .await;
        if let Err(source) = write_result {
            if file.set_len(len_before).await.is_ok() && file.flush().await.is_ok() {
                self.pending_truncate = None;
            }
            return Err(StateError::io(&self.path, source));
        }
        self.records = self.records.saturating_add(batch.records);
        Ok(())
    }

    /// `record` as one line of JSON.
    fn encode<K, V>(&self, record: &Record<K, V>) -> Result<String, StateError>
    where
        K: Serialize,
        V: Serialize,
    {
        let mut line = serde_json::to_string(record).map_err(|source| StateError::Encode {
            path: self.path.clone(),
            source,
        })?;
        line.push('\n');
        Ok(line)
    }
}

#[cfg(test)]
mod test_state;
