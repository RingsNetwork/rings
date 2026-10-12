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

use async_trait::async_trait;

use crate::error::Result;
pub use crate::storage::memory::MemStorage;

/// The authority of a store's records, fixed when the store is opened.
///
/// A store is either a cache its owner can rebuild or do without, or the only copy of state
/// whose loss would be silent (such as the transaction replay store). The law every backend
/// that takes an authority shares is the decode law: a disposable store retires a record the
/// current schema cannot decode, on the read that finds it, and reports it absent, while an
/// authoritative store reports the record and keeps it. Budget, durability and open laws are
/// per backend and documented there: the file store evicts and flushes by its authority, while
/// IndexedDB evicts its least recently accessed rows beyond its row cap under either authority,
/// so an authoritative IndexedDB store is sized never to reach its cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordAuthority {
    /// A cache its owner can rebuild or do without.
    Disposable,
    /// The only copy of its state.
    Authoritative,
}

impl RecordAuthority {
    /// Whether a read retires a record it cannot decode instead of reporting it (the decode law,
    /// shared by every backend that takes an authority).
    pub(crate) const fn retires_undecodable(self) -> bool {
        matches!(self, Self::Disposable)
    }
}

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
}

/// One record of a [`KvStorageScan::scan`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScannedRecord<V> {
    /// A whole record of `key`, filed under that key's own name.
    Filed {
        /// The record's key.
        key: String,
        /// The record's value.
        value: V,
    },
    /// Anything else found under a record's name: a record the storage cannot read or decode,
    /// or one that is not filed under its own key's name.
    Undecodable(UndecodableRecord),
}

impl std::fmt::Display for UndecodableRecord {
    /// The record's name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "record {}", self.name)
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
/// **Law (agreement).** A decoded pair is reported only under its own key's name; anything else
/// found under a record's name is undecodable for that name:
///
/// ```text
/// Filed { k, v }   ⟺  a whole record (k, v) filed under record_name(k)
/// Undecodable(u)   ⟺  anything else filed under u.name
/// ```
///
/// An owner restores exactly the `Filed` records and fails closed on the names of the rest, so
/// both methods are required: a default for either could break the agreement for a backend that
/// overrides the other.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait KvStorageScan<V>: KvStorageInterface<V> {
    /// Return every record of this storage as a [`ScannedRecord`]; a scan deletes nothing.
    async fn scan(&self) -> Result<Vec<ScannedRecord<V>>>;

    /// The name under which this storage files the record of `key`, and under which
    /// [`Self::scan`] reports whatever it finds filed there.
    fn record_name(&self, key: &str) -> String;
}

#[cfg(test)]
mod test_shared;
