#![deny(missing_docs)]

//! IndexedDB adapter with atomic updates and a row budget.
//!
//! The configured capacity counts rows, and rewriting an existing key never needs room. The
//! budget law is the authority's ([`RecordAuthority`]): when a new key would exceed the
//! capacity, a disposable store retires its least recently accessed rows first, in the same
//! transaction, and opening it with a smaller capacity retires every excess row; an
//! authoritative store retires nothing, so that write, or that open, fails with
//! `Error::StorageBudgetExhausted` and changes nothing.
//!
//! The durability law is also the authority's: a write reports success only once its
//! transaction committed, and an authoritative store opens every read-write transaction with
//! `durability: "strict"`, so a completed write survives a system crash as well as a process
//! crash; a disposable store keeps the engine's default (#912, see `request`).
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
//! An authoritative store never evicts, so recency orders nothing there: its `get` is a
//! read-only transaction that leaves the row's stamp as written. Only its writes tick the
//! clock.
//!
//! # Schema
//!
//! `SCHEMA_VERSION` 2 is the row store keyed by `key` and indexed by `access_stamp`, and the
//! clock store. The first open of a database writes the clock record `0`; every later open finds
//! it. A version-1 database (0.31 and earlier, rows ordered by wall-clock `last_visit_time`) is
//! not migrated: its rows carry no stamp, so the operator deletes it before the upgrade.
//!
//! # Record authority
//!
//! A store is opened with a [`RecordAuthority`], which fixes its decode law: a row whose payload
//! the caller's type cannot decode is retired by a disposable store and reported, and kept, by
//! an authoritative one. A disposable store holds only rows decodable as its type: `get`
//! deletes such a row in the transaction of the read that discovers it, `get_all` deletes it in
//! a follow-up transaction that deletes only a row still undecodable there, and both report it
//! absent. An authoritative store (the replay store, whose bad record keeps its stream closed)
//! reports the decode error and leaves the row untouched. The authority governs the payload
//! only; the row envelope is this store's own schema, and an envelope that does not decode is
//! an error under either authority. The budget law above is also the authority's: an
//! authoritative store refuses a new row beyond its capacity instead of evicting, since an
//! evicted record would be indistinguishable from one never written (#923).
//!
//! A `scan` (`KvStorageScan`) is outside the decode law: under either authority it reports
//! every row, a row whose envelope or payload does not decode as an `UndecodableRecord` named
//! by its key, and deletes nothing, so an owner restoring authoritative state fails closed on
//! exactly the rows it cannot read.

mod request;

use async_trait::async_trait;
use js_sys::Array;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::IdbDatabase;
use web_sys::IdbObjectStore;
use web_sys::IdbTransactionMode;

use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;
use crate::storage::RecordAuthority;
use crate::storage::ScannedRecord;
use crate::storage::UndecodableRecord;
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

/// Name of the clock store that accompanies the row store `name`.
fn clock_store_name(name: &str) -> String {
    format!("{name}{CLOCK_STORE_SUFFIX}")
}

/// One IndexedDB transaction over the row store and the clock store.
///
/// Every effect of the adapter runs inside a scope; writers await [`Scope::done`] before
/// reporting durable success (the durability law of `request`). A failed request aborts the
/// whole transaction.
struct Scope {
    /// The transaction, its commit watched from the moment it opened.
    commit: request::CommitWatch,
    /// Rows keyed by `key` and indexed by `access_stamp`.
    rows: IdbObjectStore,
    /// The single clock record under [`CLOCK_KEY`].
    clock: IdbObjectStore,
}

impl Scope {
    /// Await the commit of every request issued in this scope: under strict durability, the
    /// flushed commit.
    async fn done(self) -> Result<()> {
        self.commit.committed().await
    }

    /// The row stored under `key`, or `undefined`.
    async fn row(&self, key: &str) -> Result<JsValue> {
        request::request(
            &self
                .rows
                .get(&JsValue::from(key))
                .map_err(request::js_error)?,
        )
        .await
    }

    /// Write `row`, keyed by its inline `key`.
    async fn put_row(&self, row: &JsValue) -> Result<()> {
        request::request(&self.rows.put(row).map_err(request::js_error)?).await?;
        Ok(())
    }

    /// Delete the row stored under `key`.
    async fn delete_row(&self, key: &str) -> Result<()> {
        request::request(
            &self
                .rows
                .delete(&JsValue::from(key))
                .map_err(request::js_error)?,
        )
        .await?;
        Ok(())
    }

    /// The number of rows.
    async fn count(&self) -> Result<u32> {
        let count = request::request(&self.rows.count().map_err(request::js_error)?).await?;
        count
            .as_f64()
            .filter(|count| count.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(count))
            .map(|count| count as u32)
            .ok_or(Error::StorageCountOverflow)
    }

    /// Every row, in primary-key order.
    async fn all_rows(&self) -> Result<Vec<JsValue>> {
        let rows = request::request(&self.rows.get_all().map_err(request::js_error)?).await?;
        Ok(rows.unchecked_into::<Array>().to_vec())
    }

    /// Every primary key, in the order of [`Self::all_rows`] within one transaction.
    async fn all_keys(&self) -> Result<Vec<JsValue>> {
        let keys = request::request(&self.rows.get_all_keys().map_err(request::js_error)?).await?;
        Ok(keys.unchecked_into::<Array>().to_vec())
    }

    /// Read the clock record; `None` before the store's first start at this schema.
    async fn clock_record(&self) -> Result<Option<AccessClock>> {
        let record = request::request(
            &self
                .clock
                .get(&JsValue::from(CLOCK_KEY))
                .map_err(request::js_error)?,
        )
        .await?;
        js_value::deserialize(record)
    }

    /// Queue the write of an encoded clock value.
    async fn write_clock(&self, encoded: &JsValue) -> Result<()> {
        request::request(
            &self
                .clock
                .put_with_key(encoded, &JsValue::from(CLOCK_KEY))
                .map_err(request::js_error)?,
        )
        .await?;
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
        self.put_row(&row).await?;
        self.write_clock(&successor).await
    }

    /// Retire the `count` rows with the smallest stamps.
    ///
    /// Every candidate is decoded before any deletion is queued, so a malformed row leaves the
    /// scope with no destructive request.
    async fn evict_least_recent(&self, count: u32) -> Result<()> {
        let index = self
            .rows
            .index(ACCESS_STAMP_INDEX)
            .map_err(request::js_error)?;
        let candidates = request::request(
            &index
                .get_all_with_key_and_limit(&JsValue::NULL, count)
                .map_err(request::js_error)?,
        )
        .await?;
        let keys = candidates
            .unchecked_into::<Array>()
            .iter()
            .map(|row| js_value::deserialize::<StoredRow>(row).map(|row| row.key))
            .collect::<Result<Vec<_>>>()?;
        for key in keys {
            self.delete_row(&key).await?;
        }
        Ok(())
    }

    /// Start the clock at [`AccessClock::ORIGIN`] unless its record exists: the first open of a
    /// database writes it, and every later open finds it (module docs, schema).
    async fn start_clock(&self) -> Result<()> {
        if self.clock_record().await?.is_some() {
            return Ok(());
        }
        self.write_clock(&js_value::serialize(&AccessClock::ORIGIN)?)
            .await
    }
}

/// Named IndexedDB key/value storage with bounded row capacity.
pub struct IdbStorage {
    /// Open database handle; transactions own their requests until completion.
    db: IdbDatabase,
    /// The authority of the stored records, which fixes the decode law (module docs).
    authority: RecordAuthority,
    /// Maximum number of stored rows.
    cap: u32,
    /// Row store and database name selected by the caller.
    storage_name: String,
    /// Clock store name derived from `storage_name`.
    clock_store_name: String,
}

impl IdbStorage {
    /// Open a named disposable database with a nonzero maximum number of rows.
    ///
    /// `row_capacity` counts rows, and opening an existing store restores that row bound.
    pub async fn new_with_cap_and_name(row_capacity: u32, name: &str) -> Result<Self> {
        Self::new_with_cap_name_and_authority(row_capacity, name, RecordAuthority::Disposable).await
    }

    /// Open a named database of the given authority with a nonzero maximum number of rows (see
    /// [`Self::new_with_cap_and_name`] and the module documentation).
    pub async fn new_with_cap_name_and_authority(
        row_capacity: u32,
        name: &str,
        authority: RecordAuthority,
    ) -> Result<Self> {
        if row_capacity == 0 {
            return Err(Error::InvalidCapacity);
        }
        let clock_store_name = clock_store_name(name);
        let storage = Self {
            db: request::open(name, request::Schema {
                version: SCHEMA_VERSION,
                rows: name.to_owned(),
                key_path: "key",
                index: ACCESS_STAMP_INDEX,
                companion: clock_store_name.clone(),
            })
            .await?,
            authority,
            cap: row_capacity,
            storage_name: name.to_owned(),
            clock_store_name,
        };
        let scope = storage.scope(IdbTransactionMode::Readwrite)?;
        scope.start_clock().await?;
        scope.done().await?;
        // Opening under a smaller row budget immediately restores the configured bound.
        storage.prune().await?;
        Ok(storage)
    }

    /// Open a transaction over both object stores; a read-write one commits with the durability
    /// of this store's authority (the durability law of `request`).
    fn scope(&self, mode: IdbTransactionMode) -> Result<Scope> {
        let durability = if self.authority.flushes() {
            request::Durability::Strict
        } else {
            request::Durability::Default
        };
        let commit = request::transaction(
            &self.db,
            &[&self.storage_name, &self.clock_store_name],
            mode,
            durability,
        )?;
        let rows = commit
            .transaction()
            .object_store(&self.storage_name)
            .map_err(request::js_error)?;
        let clock = commit
            .transaction()
            .object_store(&self.clock_store_name)
            .map_err(request::js_error)?;
        Ok(Scope {
            commit,
            rows,
            clock,
        })
    }

    /// Restore the configured row budget under the budget law: a disposable store retires every
    /// least-recently-accessed excess row; an authoritative store over its budget fails.
    ///
    /// Counting, selection, and deletion share one read-write transaction.
    /// Post: `Ok(())` implies at most `cap` rows; an authoritative store removed nothing.
    async fn prune(&self) -> Result<()> {
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let count = scope.count().await?;
        if count > self.cap {
            self.make_room(&scope, count, 0).await?;
        }
        scope.done().await
    }

    /// Make room for `incoming` more rows beside the `count` stored ones under the budget law:
    /// a disposable store retires its least recently accessed rows until they fit; an
    /// authoritative store fails instead.
    ///
    /// Pre: `count + incoming > cap`, observed in `scope`.
    /// Post: `Ok(())` implies the rows that remain in `scope` plus `incoming` fit in `cap`; on
    /// `Err`, an authoritative store queued no request.
    async fn make_room(&self, scope: &Scope, count: u32, incoming: u32) -> Result<()> {
        if !self.authority.evicts() {
            return Err(Error::StorageBudgetExhausted {
                used: u64::from(count),
                required: u64::from(incoming),
                capacity: u64::from(self.cap),
            });
        }
        let excess = count.saturating_add(incoming).saturating_sub(self.cap);
        scope.evict_least_recent(excess).await
    }

    /// The decode law of this store's authority (module docs) on one row payload: the decoded
    /// value, `None` for a payload a disposable store retires, or the decode error an
    /// authoritative store reports.
    fn decode_under_authority<V>(&self, data: JsValue) -> Result<Option<V>>
    where V: DeserializeOwned {
        match js_value::deserialize(data) {
            Ok(value) => Ok(Some(value)),
            Err(_) if self.authority.retires_undecodable() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read `key` in a read-only transaction, leaving its stamp as written: the `get` of a store
    /// that never evicts, where recency orders nothing (module docs). An undecodable payload is
    /// reported under the decode law, and nothing is deleted.
    async fn get_unstamped<V>(&self, key: &str) -> Result<Option<V>>
    where V: DeserializeOwned {
        let scope = self.scope(IdbTransactionMode::Readonly)?;
        let stored = scope.row(key).await?;
        scope.done().await?;
        js_value::deserialize::<Option<StoredRow>>(stored)?
            .map(|row| js_value::deserialize(row.data))
            .transpose()
    }

    /// Delete each row of `keys` that still does not decode as `V`, in one read-write
    /// transaction: a scan found them undecodable in a read-only one, and a row rewritten
    /// between the two is the writer's, and stays.
    async fn retire_undecodable<V>(&self, keys: Vec<String>) -> Result<()>
    where V: DeserializeOwned {
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        for key in keys {
            let stored = scope.row(&key).await?;
            let still_undecodable = match js_value::deserialize::<Option<StoredRow>>(stored)? {
                Some(row) => self.decode_under_authority::<V>(row.data)?.is_none(),
                None => false,
            };
            if still_undecodable {
                scope.delete_row(&key).await?;
            }
        }
        scope.done().await
    }
}

#[async_trait(?Send)]
impl<V> KvStorageInterface<V> for IdbStorage
where V: DeserializeOwned + Serialize + Sized
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        if !self.authority.evicts() {
            return self.get_unstamped(key).await;
        }
        // Reading is a touch: keep lookup, restamp, and clock advance in one write transaction
        // so a concurrent put cannot be overwritten by stale data.
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let stored = scope.row(key).await?;
        let Some(row) = js_value::deserialize::<Option<StoredRow>>(stored)? else {
            scope.done().await?;
            return Ok(None);
        };
        // Decode before scheduling any write, so a kept row stays untouched.
        let value = match self.decode_under_authority(row.data.clone())? {
            Some(value) => value,
            None => {
                scope.delete_row(&row.key).await?;
                scope.done().await?;
                return Ok(None);
            }
        };
        scope.put_stamped(row.key, row.data).await?;
        scope.done().await?;
        Ok(Some(value))
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        // Serialize before opening a destructive transaction so serialization failure cannot
        // evict any previously stored row.
        let data = js_value::serialize(value)?;
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        // Check existence under the same transaction that will evict and store the row.
        let existing = scope.row(key).await?;
        if existing.is_undefined() || existing.is_null() {
            // Count includes every row visible to this transaction; only a new key needs room.
            let count = scope.count().await?;
            if count >= self.cap {
                self.make_room(&scope, count, 1).await?;
            }
        }
        scope.put_stamped(key.to_owned(), data).await?;
        scope.done().await
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        let scope = self.scope(IdbTransactionMode::Readonly)?;
        let entries = scope.all_rows().await?;

        let mut decoded = Vec::with_capacity(entries.len());
        let mut undecodable = Vec::new();
        for row in entries {
            let row: StoredRow = js_value::deserialize(row)?;
            match self.decode_under_authority(row.data)? {
                Some(value) => decoded.push((row.key, value)),
                None => undecodable.push(row.key),
            }
        }
        if !undecodable.is_empty() {
            self.retire_undecodable::<V>(undecodable).await?;
        }
        Ok(decoded)
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        scope.delete_row(key).await?;
        scope.done().await
    }

    /// Delete all values. The clock keeps running, so later stamps stay above earlier ones.
    async fn clear(&self) -> Result<()> {
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        request::request(&scope.rows.clear().map_err(request::js_error)?).await?;
        scope.done().await
    }

    /// Get the current storage usage.
    async fn count(&self) -> Result<u32> {
        self.scope(IdbTransactionMode::Readonly)?.count().await
    }
}

/// Rows are keyed by the record key itself, so each record is named by its key.
#[async_trait(?Send)]
impl<V> KvStorageScan<V> for IdbStorage
where V: DeserializeOwned + Serialize + Sized
{
    /// Every row, decoded or reported by its primary key; a scan deletes nothing.
    async fn scan(&self) -> Result<Vec<ScannedRecord<V>>> {
        let scope = self.scope(IdbTransactionMode::Readonly)?;
        let keys = scope.all_keys().await?;
        let rows = scope.all_rows().await?;
        Ok(keys
            .into_iter()
            .zip(rows)
            .map(|(primary_key, row)| scan_row(primary_key, row))
            .collect())
    }

    /// The key itself: a row's primary key is its record key (the key path is inline).
    fn record_name(&self, key: &str) -> String {
        key.to_owned()
    }
}

/// Decode one row of a scan as a record (pure). An undecodable row is named by its primary
/// key, which is the row's key (the key path is inline), so the name is the record's key.
fn scan_row<V>(primary_key: JsValue, row: JsValue) -> ScannedRecord<V>
where V: DeserializeOwned {
    js_value::deserialize::<StoredRow>(row)
        .and_then(|row| Ok((row.key, js_value::deserialize(row.data)?)))
        .map_or_else(
            |_| {
                ScannedRecord::Undecodable(UndecodableRecord {
                    name: primary_key
                        .as_string()
                        .unwrap_or_else(|| format!("{primary_key:?}")),
                })
            },
            |(key, value)| ScannedRecord::Filed { key, value },
        )
}

impl std::fmt::Debug for IdbStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdbStorage")
            .field("storage_name", &self.storage_name)
            .field("cap", &self.cap)
            .field("authority", &self.authority)
            .finish()
    }
}

#[cfg(test)]
#[path = "idb/tests.rs"]
mod tests;
