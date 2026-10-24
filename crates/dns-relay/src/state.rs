//! The relay's state files: the effects shared by everything the relay keeps across restarts.
//!
//! A state file is JSON, replaced atomically (written beside, then renamed over), so a crash
//! leaves either the old file or the new one, never a torn one. The address key is raw bytes,
//! created once, readable by its owner alone.

use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use rand::rngs::OsRng;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::address::AddressKey;
use crate::address::ADDRESS_KEY_LEN;

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
    /// A JSON state file does not decode, or its value does not encode.
    #[error("{path}: {source}")]
    Json {
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

    /// The JSON failure `source` on `path`.
    fn json(path: &Path, source: serde_json::Error) -> Self {
        Self::Json {
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

/// The value the JSON file `path` holds, or `None` when there is no such file.
pub(crate) async fn read_json<T>(path: &Path) -> Result<Option<T>, StateError>
where T: DeserializeOwned {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StateError::json(path, source)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StateError::io(path, error)),
    }
}

/// Replace the JSON file `path` with `value`, atomically.
pub(crate) async fn write_json<T>(path: &Path, value: &T) -> Result<(), StateError>
where T: Serialize {
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|source| StateError::json(path, source))?;
    let staging = path.with_extension("tmp");
    tokio::fs::write(&staging, bytes)
        .await
        .map_err(|source| StateError::io(&staging, source))?;
    tokio::fs::rename(&staging, path)
        .await
        .map_err(|source| StateError::io(path, source))
}

/// The address key stored at `path`, or a new random key, stored there readable by its owner
/// alone.
pub(crate) async fn load_or_create_key(path: &Path) -> Result<AddressKey, StateError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            let key = <[u8; ADDRESS_KEY_LEN]>::try_from(bytes.as_slice())
                .map_err(|_| StateError::KeyLength(path.to_path_buf()))?;
            Ok(AddressKey::from_bytes(key))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut bytes = Zeroizing::new([0u8; ADDRESS_KEY_LEN]);
            OsRng.fill_bytes(bytes.as_mut_slice());
            write_private(path, bytes.as_slice()).await?;
            Ok(AddressKey::from_bytes(*bytes))
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
