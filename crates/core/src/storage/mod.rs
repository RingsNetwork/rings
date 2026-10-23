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

/// The authority of a store's records, fixed when the store is opened: a point of the
/// durable-records lattice, the product of a crash chain (is a completed write kept across a
/// crash?) and a meaning chain (what does an absent record mean to its owner?).
///
/// ```text
///                  crash    absent means            decode    budget    durability
/// Disposable       cached   the owner's default     retire    evict     engine default
/// DurableCache     durable  the owner's default     retire    evict     survives a crash
/// Authoritative    durable  never written           report    refuse    survives a crash
/// ```
///
/// Every backend that takes an authority shares three laws, each a monotone predicate of it:
///
/// - **Decode.** A store whose absent record means the owner's default retires a record the
///   current schema cannot decode, on the read that finds it, and reports it absent; an
///   authoritative store reports the record and keeps it.
/// - **Budget (refuse, never evict).** A store whose absent record means the owner's default
///   retires its oldest records (by the backend's own order) to make room; an authoritative
///   store retires nothing, and a write of a new record into a full store, or an open under a
///   lowered budget, fails with `Error::StorageBudgetExhausted` and changes nothing. An evicted
///   authoritative record would be indistinguishable from one never written, which silently
///   rolls its state back (#923).
/// - **Durability.** A disposable store's latest writes may be lost by a crash; a durable cache
///   and an authoritative store report a write complete only once it survives a process or a
///   system crash (`FileStorage` flushes the file and its directory; `IdbStorage` commits with
///   `durability: "strict"`, #912).
///
/// The budget's unit and the open laws are per backend and documented there (`FileStorage`
/// counts bytes; `IdbStorage` counts rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordAuthority {
    /// A cache its owner can rebuild or do without, whose latest writes a crash may lose.
    Disposable,
    /// State its owner can rebuild, but whose loss costs it something (the onion entry guards,
    /// #911): every completed write survives a crash, while an absent or undecodable record
    /// still means the owner's default. A record it retires as undecodable is logged at `warn`
    /// with its name, since a crash cannot produce one: only a loss outside the store, or a
    /// schema change, can.
    DurableCache,
    /// The only copy of its state.
    Authoritative,
}

impl RecordAuthority {
    /// Whether a read retires a record it cannot decode instead of reporting it (the decode law,
    /// shared by every backend that takes an authority).
    pub(crate) const fn retires_undecodable(self) -> bool {
        !matches!(self, Self::Authoritative)
    }

    /// Whether a completed write must survive a process or a system crash (the durability law,
    /// shared by every backend that takes an authority): the file store flushes its writes, and
    /// IndexedDB commits them with strict durability.
    pub(crate) const fn flushes(self) -> bool {
        !matches!(self, Self::Disposable)
    }

    /// Whether the budget retires the oldest records to make room, instead of refusing the write
    /// (the budget law, shared by every backend that takes an authority).
    pub(crate) const fn evicts(self) -> bool {
        !matches!(self, Self::Authoritative)
    }

    /// Log the retirement of the undecodable record `name` under the decode law: at `warn` for a
    /// durable cache, whose completed writes a crash never tears, and silently otherwise.
    pub(crate) fn report_retired(self, name: &str) {
        if matches!(self, Self::DurableCache) {
            tracing::warn!(
                record = name,
                "durable cache record does not decode; retired, its owner rebuilds it"
            );
        }
    }
}

/// A record a storage cannot serve, named so that its owner can fail closed on it instead of
/// losing it: one it holds but cannot read or decode, or one it witnessed as written and no
/// longer holds.
///
/// A storage that reports such a record keeps whatever it has of it (the record, or its
/// presence marker): only its owner, or an operator, may decide that the state it held is
/// forfeit.
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
    /// Nothing found under a name the storage witnessed as written: a record lost outside the
    /// store (the presence law of an authoritative store, #915). A storage that keeps no
    /// witness never reports it.
    Missing(UndecodableRecord),
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
/// found under a record's name is undecodable for that name, and a name the storage witnessed
/// as written with nothing filed under it is missing:
///
/// ```text
/// Filed { k, v }   ⟺  a whole record (k, v) filed under record_name(k)
/// Undecodable(u)   ⟺  anything else filed under u.name
/// Missing(u)       ⟺  u.name witnessed as written, and nothing filed under it
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
