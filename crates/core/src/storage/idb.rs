#![deny(missing_docs)]

//! IndexedDB adapter with atomic updates and row-budget LRU eviction.
//!
//! The configured capacity counts rows. When a new key would exceed it, the least recently
//! accessed rows are retired first; rewriting an existing key does not change row count or
//! retire another key. Opening an existing database with a smaller capacity retires every
//! excess row in the same transaction.
//!
//! # Recency is a logical clock
//!
//! Each database keeps one store-wide access clock `c ∈ ℕ` in a companion object store.
//! A recency-updating access (every `put`, and every `get` that finds its key) runs
//!
//! ```text
//! tick : AccessClock → AccessStamp × AccessClock,   c ↦ (c, c + 1)
//! ```
//!
//! in the same read-write transaction that rewrites the accessed row with `access_stamp = c`.
//! IndexedDB runs read-write transactions over overlapping scopes one at a time, so accesses
//! form a chain `a₁ ≺ a₂ ≺ … ≺ a_k` and the clock is threaded through that chain.
//!
//! **Law (strict recency).** k accesses receive k distinct stamps `s₁ < s₂ < … < s_k`,
//! whatever the wall clock's resolution or direction; no timer is read.
//!
//! **Invariant.** Every stored stamp is below the clock, and no two rows share a stamp. The
//! `access_stamp` index therefore orders rows strictly, and eviction retires the rows with the
//! smallest stamps without any tie to break. `clear` keeps the clock, so stamps are never reused
//! within one database.
//!
//! # Schema and migration
//!
//! `SCHEMA_VERSION` 2 introduced the clock. A version-1 database (opened without a version,
//! rows ordered by wall-clock `last_visit_time`) is upgraded in place, never cleared:
//!
//! 1. the IndexedDB upgrade creates the clock store and the `access_stamp` index and drops the
//!    `last_visit_time` and `visit_count` indexes;
//! 2. the first open at version 2 finds no clock record, restamps every row with `0‥n` in its
//!    former eviction order `(last_visit_time, key)` and sets the clock to `n`, all in one
//!    transaction.
//!
//! The clock record witnesses that step 2 committed; an interrupted migration leaves the legacy
//! rows untouched and reruns on the next open. The upgrade proceeds once every connection still
//! open at version 1 (for example another tab running an older build) has closed.

use async_trait::async_trait;
use rexie::Index;
use rexie::ObjectStore;
use rexie::Rexie;
use rexie::Store;
use rexie::Transaction;
use rexie::TransactionMode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::JsValue;

use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::utils::js_value;

/// IndexedDB schema version; 2 replaced wall-clock recency with the store-wide access clock.
const SCHEMA_VERSION: u32 = 2;

/// Name and key path of the index that orders rows by access stamp, the sole eviction order.
const ACCESS_STAMP_INDEX: &str = "access_stamp";

/// Out-of-line key of the single record in the clock store.
const CLOCK_KEY: &str = "access_clock";

/// Suffix of the clock store name; appending it keeps the name distinct from the row store.
const CLOCK_STORE_SUFFIX: &str = "/access-clock";

/// Largest clock value (`Number.MAX_SAFE_INTEGER`, `2⁵³ − 1`).
///
/// IndexedDB keys and JavaScript numbers are IEEE-754 doubles, which represent every integer in
/// `0‥=CLOCK_LIMIT` exactly; beyond it distinct stamps could collapse into one index key.
const CLOCK_LIMIT: u64 = (1 << 53) - 1;

/// Logical time of one access; stamps of one database are pairwise distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
struct AccessStamp(u64);

/// Store-wide access clock: the stamp the next access receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct AccessClock(u64);

impl AccessClock {
    /// Clock of a store that has never been accessed.
    const ORIGIN: Self = Self(0);

    /// Issue the current stamp and the successor clock: `c ↦ (c, c + 1)`.
    ///
    /// Pure. For `c < CLOCK_LIMIT`, the stamp is `c` and the successor is `c + 1`, so k
    /// successive ticks yield `c, c + 1, …, c + k − 1`. At the limit it fails rather than
    /// repeat or round a stamp.
    fn tick(self) -> Result<(AccessStamp, Self)> {
        self.0
            .checked_add(1)
            .filter(|successor| *successor <= CLOCK_LIMIT)
            .map(|successor| (AccessStamp(self.0), Self(successor)))
            .ok_or(Error::IdbAccessClockExhausted(self.0))
    }
}

/// Stored row: the key, the stamp of its latest access, and the caller's payload.
///
/// The payload stays an opaque JavaScript value, so moving a row between layouts or stamps
/// never decodes or re-encodes caller data.
#[derive(Serialize, Deserialize)]
struct StoredRow {
    /// Inline IndexedDB primary key.
    key: String,
    /// Logical time of the latest access; the `access_stamp` index orders eviction by it.
    access_stamp: AccessStamp,
    /// Caller-owned payload, already serialized.
    #[serde(with = "serde_wasm_bindgen::preserve")]
    data: JsValue,
}

/// Row as written by schema version 1; unlisted fields (`visit_count`, `created_time`) drop.
#[derive(Deserialize)]
struct LegacyRow {
    /// Inline IndexedDB primary key.
    key: String,
    /// Wall-clock milliseconds of the latest access; absent sorts oldest.
    #[serde(default)]
    last_visit_time: Option<i64>,
    /// Caller-owned payload, already serialized.
    #[serde(with = "serde_wasm_bindgen::preserve")]
    data: JsValue,
}

impl LegacyRow {
    /// Position in the version-1 eviction order: its index key, then the primary key.
    fn eviction_rank(&self) -> (Option<i64>, &str) {
        (self.last_visit_time, self.key.as_str())
    }
}

/// Stamp legacy rows `0‥n` in their version-1 eviction order and return the clock `n`.
///
/// Pure. Law: the output is sorted by [`LegacyRow::eviction_rank`] and its stamps are exactly
/// `0, 1, …, n − 1`, so the relative eviction order of the rows is preserved and made strict.
fn restamp(mut rows: Vec<LegacyRow>) -> Result<(Vec<StoredRow>, AccessClock)> {
    rows.sort_by(|left, right| left.eviction_rank().cmp(&right.eviction_rank()));
    let stamped = Vec::with_capacity(rows.len());
    rows.into_iter().try_fold(
        (stamped, AccessClock::ORIGIN),
        |(mut stamped, clock), row| {
            let (access_stamp, successor) = clock.tick()?;
            stamped.push(StoredRow {
                key: row.key,
                access_stamp,
                data: row.data,
            });
            Ok((stamped, successor))
        },
    )
}

/// Name of the clock store that accompanies the row store `name`.
fn clock_store_name(name: &str) -> String {
    format!("{name}{CLOCK_STORE_SUFFIX}")
}

/// One IndexedDB transaction over the row store and the clock store.
///
/// Every effect of the adapter runs inside a scope; writers await [`Scope::done`] before
/// reporting durable success. A failed request aborts the whole transaction.
struct Scope {
    /// Completion witness of the transaction.
    transaction: Transaction,
    /// Rows keyed by `key` and indexed by `access_stamp`.
    rows: Store,
    /// The single clock record under [`CLOCK_KEY`].
    clock: Store,
}

impl Scope {
    /// Await the commit of every request issued in this scope.
    async fn done(self) -> Result<()> {
        self.transaction.done().await.map_err(Error::IDBError)
    }

    /// Read the clock record; `None` before the store's first start at this schema.
    async fn clock_record(&self) -> Result<Option<AccessClock>> {
        let record = self
            .clock
            .get(&JsValue::from(CLOCK_KEY))
            .await
            .map_err(Error::IDBError)?;
        js_value::deserialize(record)
    }

    /// Queue the write of an encoded clock value.
    async fn write_clock(&self, encoded: &JsValue) -> Result<()> {
        self.clock
            .put(encoded, Some(&JsValue::from(CLOCK_KEY)))
            .await
            .map_err(Error::IDBError)?;
        Ok(())
    }

    /// Write `data` under `key` with the next stamp and advance the clock, both in this scope.
    ///
    /// The row and the successor clock are both encoded before either write is queued, so an
    /// encoding failure changes neither; a failed request aborts both.
    async fn put_stamped(&self, key: String, data: JsValue) -> Result<()> {
        let clock = self
            .clock_record()
            .await?
            .ok_or(Error::IdbAccessClockMissing)?;
        let (access_stamp, successor) = clock.tick()?;
        let row = js_value::serialize(&StoredRow {
            key,
            access_stamp,
            data,
        })?;
        let successor = js_value::serialize(&successor)?;
        self.rows.put(&row, None).await.map_err(Error::IDBError)?;
        self.write_clock(&successor).await
    }

    /// Retire the `count` rows with the smallest stamps.
    ///
    /// Every candidate is decoded before any deletion is queued, so a malformed row leaves the
    /// scope with no destructive request.
    async fn evict_least_recent(&self, count: u32) -> Result<()> {
        let candidates = self
            .rows
            .index(ACCESS_STAMP_INDEX)
            .map_err(Error::IDBError)?
            .get_all(None, Some(count), None, None)
            .await
            .map_err(Error::IDBError)?;
        let keys = candidates
            .into_iter()
            .map(|(_stamp, row)| js_value::deserialize::<StoredRow>(row).map(|row| row.key))
            .collect::<Result<Vec<_>>>()?;
        for key in keys {
            self.rows
                .delete(&JsValue::from(key))
                .await
                .map_err(Error::IDBError)?;
        }
        Ok(())
    }

    /// Start the clock unless its record exists, migrating version-1 rows (module docs, step 2).
    ///
    /// Every legacy row is decoded, and every restamped row and the clock encoded, before any
    /// write is queued.
    async fn start_clock(&self) -> Result<()> {
        if self.clock_record().await?.is_some() {
            return Ok(());
        }
        let legacy = self
            .rows
            .get_all(None, None, None, None)
            .await
            .map_err(Error::IDBError)?
            .into_iter()
            .map(|(_key, row)| js_value::deserialize::<LegacyRow>(row))
            .collect::<Result<Vec<_>>>()?;
        let (restamped, clock) = restamp(legacy)?;
        let encoded = restamped
            .iter()
            .map(js_value::serialize)
            .collect::<Result<Vec<_>>>()?;
        let clock = js_value::serialize(&clock)?;
        for row in encoded {
            self.rows.put(&row, None).await.map_err(Error::IDBError)?;
        }
        self.write_clock(&clock).await
    }
}

/// Named IndexedDB key/value storage with bounded row capacity.
pub struct IdbStorage {
    /// Open database handle; transactions own their requests until completion.
    db: Rexie,
    /// Maximum number of stored rows.
    cap: u32,
    /// Row store and database name selected by the caller.
    storage_name: String,
    /// Clock store name derived from `storage_name`.
    clock_store_name: String,
}

impl IdbStorage {
    /// Open a named database with a nonzero maximum number of rows.
    ///
    /// `row_capacity` counts rows, and opening an existing store restores that row bound.
    /// A database of an older schema is migrated in place without losing rows (module docs).
    pub async fn new_with_cap_and_name(row_capacity: u32, name: &str) -> Result<Self> {
        if row_capacity == 0 {
            return Err(Error::InvalidCapacity);
        }
        let clock_store_name = clock_store_name(name);
        let storage = Self {
            db: Rexie::builder(name)
                .version(SCHEMA_VERSION)
                .add_object_store(
                    ObjectStore::new(name)
                        .key_path("key")
                        .auto_increment(false)
                        .add_index(Index::new(ACCESS_STAMP_INDEX, ACCESS_STAMP_INDEX)),
                )
                .add_object_store(ObjectStore::new(&clock_store_name))
                .build()
                .await
                .map_err(Error::IDBError)?,
            cap: row_capacity,
            storage_name: name.to_owned(),
            clock_store_name,
        };
        let scope = storage.scope(TransactionMode::ReadWrite)?;
        scope.start_clock().await?;
        scope.done().await?;
        // Opening under a smaller row budget immediately restores the configured bound.
        storage.prune().await?;
        Ok(storage)
    }

    /// Open a transaction over both object stores.
    fn scope(&self, mode: TransactionMode) -> Result<Scope> {
        let transaction = self
            .db
            .transaction(&[&self.storage_name, &self.clock_store_name], mode)
            .map_err(Error::IDBError)?;
        let rows = transaction
            .store(&self.storage_name)
            .map_err(Error::IDBError)?;
        let clock = transaction
            .store(&self.clock_store_name)
            .map_err(Error::IDBError)?;
        Ok(Scope {
            transaction,
            rows,
            clock,
        })
    }

    /// Restore the configured row budget by retiring every least-recently-accessed excess row.
    ///
    /// Counting, selection, and deletion share one read-write transaction.
    async fn prune(&self) -> Result<()> {
        let scope = self.scope(TransactionMode::ReadWrite)?;
        let count = scope.rows.count(None).await.map_err(Error::IDBError)?;
        if count > self.cap {
            scope
                .evict_least_recent(count.saturating_sub(self.cap))
                .await?;
        }
        scope.done().await
    }

    /// Delete all values. The clock keeps running, so later stamps stay above earlier ones.
    pub async fn clear(&self) -> Result<()> {
        let scope = self.scope(TransactionMode::ReadWrite)?;
        scope.rows.clear().await.map_err(Error::IDBError)?;
        scope.done().await
    }

    /// Get the current storage usage.
    pub async fn count(&self) -> Result<u32> {
        let scope = self.scope(TransactionMode::ReadOnly)?;
        scope.rows.count(None).await.map_err(Error::IDBError)
    }
}

#[async_trait(?Send)]
impl<V> KvStorageInterface<V> for IdbStorage
where V: DeserializeOwned + Serialize + Sized
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        // Reading is a touch: keep lookup, restamp, and clock advance in one write transaction
        // so a concurrent put cannot be overwritten by stale data.
        let scope = self.scope(TransactionMode::ReadWrite)?;
        let stored = scope
            .rows
            .get(&JsValue::from(key))
            .await
            .map_err(Error::IDBError)?;
        let Some(row) = js_value::deserialize::<Option<StoredRow>>(stored)? else {
            scope.done().await?;
            return Ok(None);
        };
        // Decode before scheduling any write so malformed values remain untouched.
        let value = js_value::deserialize(row.data.clone())?;
        scope.put_stamped(row.key, row.data).await?;
        scope.done().await?;
        Ok(Some(value))
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        // Serialize before opening a destructive transaction so serialization failure cannot
        // evict any previously stored row.
        let data = js_value::serialize(value)?;
        let scope = self.scope(TransactionMode::ReadWrite)?;
        // Check existence under the same transaction that will evict and store the row.
        let existing = scope
            .rows
            .get(&JsValue::from(key))
            .await
            .map_err(Error::IDBError)?;
        if existing.is_undefined() || existing.is_null() {
            // Count includes every row visible to this transaction; only a new key needs room.
            let count = scope.rows.count(None).await.map_err(Error::IDBError)?;
            if count >= self.cap {
                // Remove every row needed to make room, rather than only the first candidate.
                let excess = count.saturating_sub(self.cap).saturating_add(1);
                scope.evict_least_recent(excess).await?;
            }
        }
        scope.put_stamped(key.to_owned(), data).await?;
        scope.done().await
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let scope = self.scope(TransactionMode::ReadOnly)?;
        let entries = scope
            .rows
            .get_all(None, None, None, None)
            .await
            .map_err(Error::IDBError)?;

        entries
            .into_iter()
            .map(|(_key, row)| {
                let row: StoredRow = js_value::deserialize(row)?;
                Ok((row.key, js_value::deserialize(row.data)?))
            })
            .collect()
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let scope = self.scope(TransactionMode::ReadWrite)?;
        scope
            .rows
            .delete(&JsValue::from(key))
            .await
            .map_err(Error::IDBError)?;
        scope.done().await
    }

    async fn clear(&self) -> Result<()> {
        IdbStorage::clear(self).await
    }

    async fn count(&self) -> Result<u32> {
        IdbStorage::count(self).await
    }
}

impl std::fmt::Debug for IdbStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdbStorage")
            .field("storage_name", &self.storage_name)
            .field("cap", &self.cap)
            .finish()
    }
}

#[cfg(test)]
#[path = "idb/tests.rs"]
mod tests;
