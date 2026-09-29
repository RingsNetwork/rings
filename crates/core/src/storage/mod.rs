//! Key-value storage adapters and their shared behavioral contract.
//!
//! Capacity is backend-specific and always counts the unit named by its constructor:
//! `MemStorage::bounded` and `IdbStorage` count keys/rows, while `FileStorage` counts
//! serialized record bytes. Bounded adapters preserve their
//! documented eviction order; memory and file use write recency, while IndexedDB uses access
//! recency. Replacing one adapter with another can therefore change both budget units and
//! retention order.

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
/// Persistent storage for native runtimes.
pub mod file;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// IndexedDB-backed storage for browser runtimes.
pub mod idb;
/// In-memory key value storage.
pub mod memory;
mod write_ordered;

use std::sync::Arc;

use async_trait::async_trait;
use rings_runtime::MaybeSendSync;

use crate::error::Result;
pub use crate::storage::memory::MemStorage;

/// A record a storage holds but cannot read or decode, named so that its owner can fail closed
/// on it instead of losing it.
///
/// A storage that reports such a record keeps it: only its owner, or an operator, may decide
/// that the state it held is forfeit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndecodableRecord {
    /// The name the record is filed under: [`KvStorageScan::record_name`] of the key it is
    /// filed as, which is the key itself or the backend's image of it (`FileStorage`: the file
    /// name).
    pub name: String,
    /// What the record reveals of the key it belongs to.
    pub identity: RecordIdentity,
}

/// What an unreadable or undecodable record reveals of the key it belongs to; the cases are
/// exclusive, so a record is filed as its own key or carries another, never both.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordIdentity {
    /// No key could be read from the record.
    Unreadable,
    /// The record names the key it is filed as (its key is intact and names this record's
    /// name): a torn or corrupt record of that key.
    FiledAs(String),
    /// The record names another key than the one it is filed as, whole or torn: a misfiled
    /// record, copied or moved over another's file, which its owner may have to fail closed
    /// under that key as well.
    Carries(String),
}

/// One record of a [`KvStorageScan::scan`]: its key and value, or the record the storage
/// holds but cannot decode.
pub type ScannedRecord<V> = std::result::Result<(String, V), UndecodableRecord>;

impl std::fmt::Display for UndecodableRecord {
    /// The record's name, and the key it is filed as, the key it carries when misfiled, or
    /// that its key is unreadable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.identity {
            RecordIdentity::Unreadable => write!(f, "record {} (key unreadable)", self.name),
            RecordIdentity::FiledAs(key) => write!(f, "record {} (key {key})", self.name),
            RecordIdentity::Carries(key) => {
                write!(f, "record {} (misfiled: holds key {key})", self.name)
            }
        }
    }
}

/// Backend-neutral operations over stored key-value pairs.
///
/// This interface does not imply a common capacity unit, eviction order, or overwrite effect.
/// Each bounded implementation documents whether its limit counts rows, keys, or serialized
/// bytes, how it chooses entries to retire, and how replacing a value affects the budget.
/// Backend-specific failure guarantees are documented by each implementation.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait KvStorageInterface<V> {
    /// Get a cache entry by `key`.
    async fn get(&self, key: &str) -> Result<Option<V>>;

    /// Put `entry` in the cache under `key`.
    async fn put(&self, key: &str, value: &V) -> Result<()>;

    /// Return every key value pair in this storage.
    async fn get_all(&self) -> Result<Vec<(String, V)>>;

    /// Remove an `entry` by `key`.
    async fn remove(&self, key: &str) -> Result<()>;

    /// Delete all values.
    async fn clear(&self) -> Result<()>;

    /// Get the current storage usage.
    async fn count(&self) -> Result<u32>;
}

/// A key-value storage that enumerates its records one by one, reporting those it cannot read
/// or decode instead of failing or deleting, together with the naming that ties a reported
/// record back to its key.
///
/// **Law (agreement).** A scan and the naming agree in three clauses: a decoded pair is the
/// record filed under its own key's name; a record that cannot be read, decoded or tied to its
/// key is reported under the name it is filed under; and a reported record that names another
/// key than the one it is filed as says so:
///
/// ```text
/// scan ∋ Ok((k, v))                             ⟹  v is the record filed under record_name(k)
/// scan ∋ Err(u) ∧ u is filed as key k           ⟹  u.name = record_name(k)
/// scan ∋ Err(u) ∧ u names key k, record_name(k) ≠ u.name  ⟹  u.identity = Carries(k)
/// ```
///
/// An owner restores exactly the pairs a scan decoded, fails closed on exactly the names it
/// reported, and fails closed on the keys reported as carried whose own record did not restore,
/// so both methods are required: a default for either could break the agreement for a backend
/// that overrides the other. A backend keyed by the key itself (memory, IndexedDB) satisfies
/// the first and third clauses by construction; `FileStorage`, which files by digest, reports a
/// record filed under another key's name as `Carries`.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait KvStorageScan<V>: KvStorageInterface<V> {
    /// Return every record of this storage, each decoded or reported as an
    /// [`UndecodableRecord`]; a scan deletes nothing.
    async fn scan(&self) -> Result<Vec<ScannedRecord<V>>>;

    /// The name under which this storage files the record of `key`, and under which
    /// [`Self::scan`] reports that record when it cannot read or decode it.
    fn record_name(&self, key: &str) -> String;
}

/// A shared storage is the storage it shares: every operation delegates to it, so a wrapper
/// never restates (or mistakes) the naming its scan must agree with.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<V, S> KvStorageInterface<V> for Arc<S>
where
    V: MaybeSendSync,
    S: KvStorageInterface<V> + MaybeSendSync + ?Sized,
{
    /// `get` of the shared storage.
    async fn get(&self, key: &str) -> Result<Option<V>> {
        self.as_ref().get(key).await
    }

    /// `put` of the shared storage.
    async fn put(&self, key: &str, value: &V) -> Result<()> {
        self.as_ref().put(key, value).await
    }

    /// `get_all` of the shared storage.
    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        self.as_ref().get_all().await
    }

    /// `remove` of the shared storage.
    async fn remove(&self, key: &str) -> Result<()> {
        self.as_ref().remove(key).await
    }

    /// `clear` of the shared storage.
    async fn clear(&self) -> Result<()> {
        self.as_ref().clear().await
    }

    /// `count` of the shared storage.
    async fn count(&self) -> Result<u32> {
        self.as_ref().count().await
    }
}

/// A shared scannable storage scans and names exactly as the storage it shares.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<V, S> KvStorageScan<V> for Arc<S>
where
    V: MaybeSendSync,
    S: KvStorageScan<V> + MaybeSendSync + ?Sized,
{
    /// `scan` of the shared storage.
    async fn scan(&self) -> Result<Vec<ScannedRecord<V>>> {
        self.as_ref().scan().await
    }

    /// `record_name` of the shared storage.
    fn record_name(&self, key: &str) -> String {
        self.as_ref().record_name(key)
    }
}
