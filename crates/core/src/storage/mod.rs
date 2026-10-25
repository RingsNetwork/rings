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
/// Retained         durable  never written, or       retire    refuse    survives a crash
///                           retired by its owner
/// Authoritative    durable  never written           report    refuse    survives a crash
/// ```
///
/// Every backend that takes an authority shares three laws, each a monotone predicate of it:
///
/// - **Decode.** A store below the authoritative meaning retires a record the current schema
///   cannot decode, on the read that finds it, and reports it absent; an authoritative store
///   reports the record and keeps it.
/// - **Budget (refuse, never evict).** A store whose absent record means the owner's default
///   retires its oldest records (by the backend's own order) to make room; a retained or an
///   authoritative store retires nothing, and a write of a new record into a full store, or an
///   open under a lowered budget, fails with `Error::StorageBudgetExhausted` and changes
///   nothing. An evicted retained record would be a loss its owner never chose (held mail), and
///   an evicted authoritative one would be indistinguishable from one never written, which
///   silently rolls its state back (#923).
/// - **Durability.** A disposable store's latest writes may be lost by a crash; every other
///   store reports a write complete only once it survives a process or a system crash (`FileStorage` flushes the file and its directory; `IdbStorage` commits with
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
    /// The only copy of state whose loss is a liveness failure, not a safety one (the relay
    /// inbox carriers, #1021): every completed write survives a crash and nothing is evicted,
    /// so a record is gone only once its owner retired it, while a record that does not decode
    /// is retired (and logged), since failing it closed would protect nothing.
    Retained,
    /// The only copy of its state.
    Authoritative,
}

/// The crash chain of the durable-records lattice: is a completed write kept across a crash?
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Crash {
    /// Lost at every restart (an in-memory store).
    Volatile,
    /// Kept unless a crash loses the latest writes or tears a record.
    Cached,
    /// Kept: a completed write or removal survives a process or a system crash.
    Durable,
}

/// The meaning chain of the durable-records lattice: what does an absent record mean to its
/// owner?
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Meaning {
    /// The owner's default: any record may be dropped and rebuilt.
    Rebuild,
    /// Never written, or retired by the owner: a loss is a liveness failure.
    Retained,
    /// Never written: a loss is a safety failure.
    Authoritative,
}

impl RecordAuthority {
    /// The authority's two components, its point of the lattice (pure). Every law below is a
    /// function of them, never of the variant, so a new point inherits the laws its components
    /// imply.
    pub const fn components(self) -> (Crash, Meaning) {
        match self {
            Self::Disposable => (Crash::Cached, Meaning::Rebuild),
            Self::DurableCache => (Crash::Durable, Meaning::Rebuild),
            Self::Retained => (Crash::Durable, Meaning::Retained),
            Self::Authoritative => (Crash::Durable, Meaning::Authoritative),
        }
    }

    /// The authority's crash component.
    pub const fn crash(self) -> Crash {
        self.components().0
    }

    /// The authority's meaning component.
    pub const fn meaning(self) -> Meaning {
        self.components().1
    }

    /// Whether a read retires a record it cannot decode instead of reporting it (the decode law,
    /// shared by every backend that takes an authority): every meaning below `Authoritative`
    /// retires; only an authoritative owner, whose absent record means "never written", needs
    /// the record kept (fail closed).
    pub(crate) const fn retires_undecodable(self) -> bool {
        !matches!(self.meaning(), Meaning::Authoritative)
    }

    /// Whether a completed write must survive a process or a system crash (the durability law,
    /// shared by every backend that takes an authority): `crash = Durable`. The file store
    /// flushes its writes, and IndexedDB commits them with strict durability.
    pub(crate) const fn flushes(self) -> bool {
        matches!(self.crash(), Crash::Durable)
    }

    /// Whether the budget retires the oldest records to make room, instead of refusing the write
    /// (the budget law, shared by every backend that takes an authority): `meaning = Rebuild`,
    /// since an evicted record of any higher meaning is a loss.
    pub(crate) const fn evicts(self) -> bool {
        matches!(self.meaning(), Meaning::Rebuild)
    }

    /// Whether the store keeps a witness of every record's presence (the presence law):
    /// `meaning = Authoritative`, the one meaning under which an absent record must be told
    /// apart from one never written. Only the native file store keeps markers: IndexedDB loses
    /// no proper subset of a database's rows and evicts no authoritative row, so it needs none
    /// (durable-records, Prop. 3), and the predicate exists only where `FileStorage` does.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) const fn witnesses(self) -> bool {
        matches!(self.meaning(), Meaning::Authoritative)
    }

    /// Log the retirement of the undecodable record `name` under the decode law: at `warn` for a
    /// durable store, whose completed writes a crash never tears, so that only a loss outside the
    /// store or a schema change can explain it, and silently otherwise.
    pub(crate) fn report_retired(self, name: &str) {
        if self.flushes() {
            tracing::warn!(
                record = name,
                "durable record does not decode; retired, its owner rebuilds it"
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

    /// Store every pair of `records` at once: one flush, or one commit, for the whole batch
    /// instead of one per record (group commit, #916; durable-records, Law Group commit).
    ///
    /// **Law (batch).** A later pair of one key supersedes an earlier one. `Ok` means every
    /// record is stored and, in a durable store, survives a crash (the durability law). A crash,
    /// or an `Err`, leaves each record whole at its previous or its new value, with any subset
    /// of them new and no order promised across keys; a store that refuses (never evicts)
    /// refuses a batch that does not fit its budget whole, changing nothing. Required, as the
    /// other two methods are: a default of one `put` per record would keep the law but pay one
    /// flush per record, which is what the batch exists to avoid.
    async fn put_batch(&self, records: &[(String, V)]) -> Result<()>;
}

#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
mod test_authority;
#[cfg(test)]
mod test_shared;
