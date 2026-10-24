//! The relay's state files: the effects shared by everything the relay keeps across restarts.
//!
//! - The address key is raw bytes, created once, readable by its owner alone.
//! - The address book and the memory are [`Journal`]s: a persistent map kept as an append-only
//!   log of puts and removes, one JSON record per line, compacted into a snapshot once the log
//!   holds more than twice the live entries. A change costs one appended line, whatever the
//!   size of the map.
//!
//! Both journals hold caches whose loss degrades gracefully (a client resolves a name again, an
//! owner is looked up again), so a record is written before its change returns but not synced
//! to the disk, and replay stops at the first line that does not decode: a crash costs at most
//! the last few records. A compaction's snapshot is synced before it replaces the log, so a crash
//! never costs the map.

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

/// A persistent map kept as an append-only log (see the module doc).
///
/// ```text
///   replay(log) = fold apply ∅ log        apply(m, Put k v) = m[k ↦ v],  apply(m, Remove k) = m ∖ k
///   Law: replay(log ++ [r]) = apply(replay(log), r);  replay(compact(m)) = m
/// ```
pub(crate) struct Journal {
    path: PathBuf,
    records: usize,
}

impl Journal {
    /// Open the journal at `path` and replay it; a missing journal is an empty map.
    pub(crate) async fn open<K, V>(path: &Path) -> Result<(Self, BTreeMap<K, V>), StateError>
    where
        K: Ord + DeserializeOwned,
        V: DeserializeOwned,
    {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => return Err(StateError::io(path, error)),
        };
        let mut map = BTreeMap::new();
        let mut records = 0usize;
        for line in text.lines() {
            match serde_json::from_str::<Record<K, V>>(line) {
                Ok(Record::Put(key, value)) => {
                    map.insert(key, value);
                }
                Ok(Record::Remove(key)) => {
                    map.remove(&key);
                }
                Err(error) => {
                    tracing::warn!("{}: replay stops at a torn record: {error}", path.display());
                    break;
                }
            }
            records = records.saturating_add(1);
        }
        let journal = Self {
            path: path.to_path_buf(),
            records,
        };
        Ok((journal, map))
    }

    /// Append the record that `key` maps to `value`.
    pub(crate) async fn put<K, V>(&mut self, key: &K, value: &V) -> Result<(), StateError>
    where
        K: Serialize,
        V: Serialize,
    {
        self.append(&Record::Put(key, value)).await
    }

    /// Append the record that `key` maps to nothing.
    pub(crate) async fn remove<K>(&mut self, key: &K) -> Result<(), StateError>
    where K: Serialize {
        self.append(&Record::<&K, ()>::Remove(key)).await
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
        Ok(())
    }

    /// Append `record` as one line.
    async fn append<K, V>(&mut self, record: &Record<K, V>) -> Result<(), StateError>
    where
        K: Serialize,
        V: Serialize,
    {
        let line = self.encode(record)?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|source| StateError::io(&self.path, source))?;
        // A Tokio file writes in the background: the flush waits until the line is written, so
        // no later write or compaction overtakes it.
        file.write_all(line.as_bytes())
            .await
            .map_err(|source| StateError::io(&self.path, source))?;
        file.flush()
            .await
            .map_err(|source| StateError::io(&self.path, source))?;
        self.records = self.records.saturating_add(1);
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
