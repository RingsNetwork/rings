#![deny(missing_docs)]

//! IndexedDB adapter with atomic updates and a row budget.
//!
//! The configured capacity counts rows, and rewriting an existing key never needs room. The
//! budget law is the authority's ([`RecordAuthority`]): when a new key would exceed the
//! capacity, a disposable store or a durable cache retires its least recently accessed rows
//! first, in the same transaction, and opening it with a smaller capacity retires every excess
//! row; an authoritative store retires nothing, so that write, or that open, fails with
//! `Error::StorageBudgetExhausted` and changes nothing.
//!
//! The durability law is also the authority's: a write reports success only once its
//! transaction committed, and a durable cache or an authoritative store opens every read-write
//! transaction with `durability: "strict"`, so a completed write survives a system crash as well
//! as a process crash; a disposable store keeps the engine's default (#912, see `request`).
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
//! the caller's type cannot decode is retired by a disposable store or a durable cache (which
//! logs it at `warn`), and reported, and kept, by an authoritative one. A store that retires
//! holds only rows decodable as its type: `get` deletes such a row in the transaction of the
//! read that discovers it, `get_all` deletes it in a follow-up transaction that deletes only a
//! row still undecodable there, and both report it absent. An authoritative store (the replay store, whose bad record keeps its stream closed)
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

/// Out-of-line key of the byte-usage counter in the companion store when `byte_cap` is enabled.
const USED_BYTES_KEY: &str = "used_bytes";

/// Suffix of the clock store name; appending it keeps the name distinct from the row store.
const CLOCK_STORE_SUFFIX: &str = "/access-clock";

/// Largest clock value (`Number.MAX_SAFE_INTEGER`, `2⁵³ − 1`).
///
/// IndexedDB keys and JavaScript numbers are IEEE-754 doubles, which represent every integer in
/// `0‥=CLOCK_LIMIT` exactly; beyond it distinct stamps could collapse into one index key.
const CLOCK_LIMIT: u64 = (1 << 53) - 1;

/// Logical time of one access; stamps of one database are pairwise distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(transparent)]
struct AccessStamp(u64);

impl Serialize for AccessStamp {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: serde::Serializer {
        if self.0 <= CLOCK_LIMIT {
            serializer.serialize_f64(self.0 as f64)
        } else {
            Err(serde::ser::Error::custom(
                "IndexedDB access stamp exceeded IEEE-754 exact integer range",
            ))
        }
    }
}

/// Store-wide access clock: the stamp the next access receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
struct AccessClock(u64);

impl Serialize for AccessClock {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: serde::Serializer {
        if self.0 <= CLOCK_LIMIT {
            serializer.serialize_f64(self.0 as f64)
        } else {
            Err(serde::ser::Error::custom(
                "IndexedDB access clock exceeded IEEE-754 exact integer range",
            ))
        }
    }
}

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

/// Stored row: the key, the stamp of its latest access, optional byte accounting, and payload.
///
/// The payload stays an opaque JavaScript value, so moving a row between layouts or stamps
/// never decodes or re-encodes caller data.
#[derive(Serialize, Deserialize)]
struct StoredRow {
    /// Inline IndexedDB primary key.
    key: String,
    /// Logical time of the latest access; the `access_stamp` index orders eviction by it.
    access_stamp: AccessStamp,
    /// Canonical wire byte size recorded when this store enforces a byte budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    byte_len: Option<u32>,
    /// Caller-owned payload, already serialized.
    #[serde(with = "serde_wasm_bindgen::preserve")]
    data: JsValue,
}

impl StoredRow {
    fn accounted_bytes(&self) -> u64 {
        u64::from(self.byte_len.unwrap_or(0))
    }
}

fn measure_wire_bytes<V: Serialize>(key: &str, value: &V) -> Result<u32> {
    let bytes = rings_codec::serialize(&(key, value)).map_err(Error::CodecSerialize)?;
    u32::try_from(bytes.len()).map_err(|_| Error::StorageCountOverflow)
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

    /// Queue the write of `row`, keyed by its inline `key`.
    fn queue_put_row(&self, row: &JsValue) -> Result<web_sys::IdbRequest> {
        self.rows.put(row).map_err(request::js_error)
    }

    /// Queue the deletion of the row stored under `key`.
    fn queue_delete_row(&self, key: &str) -> Result<web_sys::IdbRequest> {
        self.rows
            .delete(&JsValue::from(key))
            .map_err(request::js_error)
    }

    /// Delete the row stored under `key`.
    async fn delete_row(&self, key: &str) -> Result<()> {
        request::request(&self.queue_delete_row(key)?).await?;
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
    fn queue_write_clock(&self, encoded: &JsValue) -> Result<web_sys::IdbRequest> {
        self.clock
            .put_with_key(encoded, &JsValue::from(CLOCK_KEY))
            .map_err(request::js_error)
    }

    /// Write an encoded clock value.
    async fn write_clock(&self, encoded: &JsValue) -> Result<()> {
        request::request(&self.queue_write_clock(encoded)?).await?;
        Ok(())
    }

    /// Read the byte-usage counter from the companion store; `None` before byte tracking starts.
    async fn used_bytes_record(&self) -> Result<Option<u64>> {
        let record = request::request(
            &self
                .clock
                .get(&JsValue::from(USED_BYTES_KEY))
                .map_err(request::js_error)?,
        )
        .await?;
        js_value::deserialize(record)
    }

    /// Queue the write of the byte-usage counter.
    fn queue_write_used_bytes(&self, used_bytes: u64) -> Result<web_sys::IdbRequest> {
        let encoded = js_value::serialize_storage(&used_bytes)?;
        self.clock
            .put_with_key(&encoded, &JsValue::from(USED_BYTES_KEY))
            .map_err(request::js_error)
    }

    /// Queue the writes of `rows`, optional `clock`, and optional `used_bytes`, and await all of
    /// them in one batch.
    async fn put_rows_with_clock_and_bytes(
        &self,
        rows: &[JsValue],
        clock: Option<&JsValue>,
        used_bytes: Option<u64>,
    ) -> Result<()> {
        let mut requests = Vec::with_capacity(
            rows.len() + usize::from(clock.is_some()) + usize::from(used_bytes.is_some()),
        );
        for row in rows {
            requests.push(self.queue_put_row(row)?);
        }
        if let Some(encoded) = clock {
            requests.push(self.queue_write_clock(encoded)?);
        }
        if let Some(bytes) = used_bytes {
            requests.push(self.queue_write_used_bytes(bytes)?);
        }
        request::requests(&requests).await
    }

    /// Write `data` under `key` with the next stamp and advance the clock, both queued and
    /// watched in the same synchronous turn before awaiting either request.
    ///
    /// The row and the successor clock are both encoded before either write is queued, so an
    /// encoding failure changes neither; a failed request or a dropped scope aborts both.
    async fn put_stamped(
        &self,
        key: String,
        byte_len: Option<u32>,
        data: JsValue,
        used_bytes: Option<u64>,
    ) -> Result<()> {
        let clock = self
            .clock_record()
            .await?
            .ok_or(Error::IdbAccessClockMissing)?;
        let (access_stamp, successor) = clock.tick()?;
        let row = js_value::serialize_storage(&StoredRow {
            key,
            access_stamp,
            byte_len,
            data,
        })?;
        let successor = js_value::serialize_storage(&successor)?;
        self.put_rows_with_clock_and_bytes(&[row], Some(&successor), used_bytes)
            .await
    }

    /// Return the least-recently-accessed rows not in `excluded` until `min_rows` and `min_bytes`
    /// are both satisfied.
    ///
    /// Every candidate is decoded before any deletion is queued, so a malformed row leaves the
    /// scope with no destructive request.
    async fn least_recent_rows_for_budget(
        &self,
        excluded: &std::collections::BTreeSet<&str>,
        min_rows: u32,
        min_bytes: u64,
    ) -> Result<(Vec<String>, u64)> {
        if min_rows == 0 && min_bytes == 0 {
            return Ok((Vec::new(), 0));
        }
        let index = self
            .rows
            .index(ACCESS_STAMP_INDEX)
            .map_err(request::js_error)?;
        let candidates = if min_bytes == 0 {
            let excluded_len =
                u32::try_from(excluded.len()).map_err(|_| Error::StorageCountOverflow)?;
            let limit = min_rows.saturating_add(excluded_len);
            request::request(
                &index
                    .get_all_with_key_and_limit(&JsValue::NULL, limit)
                    .map_err(request::js_error)?,
            )
            .await?
        } else {
            request::request(&index.get_all().map_err(request::js_error)?).await?
        };
        let mut keys = Vec::new();
        let mut reclaimed_bytes = 0_u64;
        for row in candidates.unchecked_into::<Array>().iter() {
            let row = js_value::deserialize::<StoredRow>(row)?;
            if !excluded.contains(row.key.as_str()) {
                reclaimed_bytes = reclaimed_bytes.saturating_add(row.accounted_bytes());
                keys.push(row.key);
                if keys.len() >= min_rows as usize && reclaimed_bytes >= min_bytes {
                    break;
                }
            }
        }
        Ok((keys, reclaimed_bytes))
    }

    /// Start the clock at [`AccessClock::ORIGIN`] unless its record exists, and initialize the
    /// byte counter from stored rows when `track_bytes` is enabled.
    async fn start_clock(&self, track_bytes: bool) -> Result<()> {
        if self.clock_record().await?.is_none() {
            self.write_clock(&js_value::serialize_storage(&AccessClock::ORIGIN)?)
                .await?;
        }
        if track_bytes && self.used_bytes_record().await?.is_none() {
            let mut sum = 0_u64;
            for raw in self.all_rows().await? {
                if let Ok(row) = js_value::deserialize::<StoredRow>(raw) {
                    sum = sum.saturating_add(row.accounted_bytes());
                }
            }
            request::request(&self.queue_write_used_bytes(sum)?).await?;
        }
        Ok(())
    }
}

/// Named IndexedDB key/value storage with bounded row capacity and optional byte budget.
pub struct IdbStorage {
    /// Open database handle; transactions own their requests until completion.
    db: IdbDatabase,
    /// The authority of the stored records, which fixes the decode law (module docs).
    authority: RecordAuthority,
    /// Maximum number of stored rows.
    cap: u32,
    /// Optional maximum total canonical wire bytes across stored rows.
    byte_cap: Option<u32>,
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
        Self::open_internal(row_capacity, None, name, authority).await
    }

    /// Open a named database of the given authority enforcing both a nonzero row cap and a nonzero
    /// canonical wire byte cap (`byte_capacity`), matching `FileStorage` byte-budget semantics.
    pub async fn new_with_byte_cap_name_and_authority(
        row_capacity: u32,
        byte_capacity: u32,
        name: &str,
        authority: RecordAuthority,
    ) -> Result<Self> {
        if byte_capacity == 0 {
            return Err(Error::InvalidCapacity);
        }
        Self::open_internal(row_capacity, Some(byte_capacity), name, authority).await
    }

    async fn open_internal(
        row_capacity: u32,
        byte_capacity: Option<u32>,
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
            byte_cap: byte_capacity,
            storage_name: name.to_owned(),
            clock_store_name,
        };
        let scope = storage.scope(IdbTransactionMode::Readwrite)?;
        scope.start_clock(storage.byte_cap.is_some()).await?;
        scope.done().await?;
        // Opening under a smaller row or byte budget immediately restores the configured bound.
        storage.prune().await?;
        Ok(storage)
    }

    /// Open a transaction over both object stores; a read-write one commits with the durability
    /// of this store's authority (the durability law of `request`).
    fn scope(&self, mode: IdbTransactionMode) -> Result<Scope> {
        let durability =
            if matches!(mode, IdbTransactionMode::Readwrite) && self.authority.flushes() {
                request::Durability::Strict
            } else {
                request::Durability::Default
            };
        self.scope_with_durability(mode, durability)
    }

    fn scope_with_durability(
        &self,
        mode: IdbTransactionMode,
        durability: request::Durability,
    ) -> Result<Scope> {
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

    /// Restore the configured row and byte budgets under the budget law: a disposable store
    /// retires every least-recently-accessed excess row; an authoritative store over its budget
    /// fails.
    async fn prune(&self) -> Result<()> {
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let count = scope.count().await?;
        let used_bytes = match self.byte_cap {
            Some(_) => scope.used_bytes_record().await?.unwrap_or(0),
            None => 0,
        };
        let excess_rows = count.saturating_sub(self.cap);
        let excess_bytes = self
            .byte_cap
            .map_or(0, |cap| used_bytes.saturating_sub(u64::from(cap)));
        if excess_rows > 0 || excess_bytes > 0 {
            if !self.authority.evicts() {
                if excess_rows > 0 {
                    return Err(Error::StorageBudgetExhausted {
                        used: u64::from(count),
                        required: 0,
                        capacity: u64::from(self.cap),
                    });
                }
                let byte_cap = u64::from(self.byte_cap.unwrap_or(0));
                return Err(Error::StorageBudgetExhausted {
                    used: used_bytes,
                    required: 0,
                    capacity: byte_cap,
                });
            }
            let (evicted_keys, reclaimed_bytes) = scope
                .least_recent_rows_for_budget(
                    &std::collections::BTreeSet::new(),
                    excess_rows,
                    excess_bytes,
                )
                .await?;
            let mut requests = Vec::with_capacity(evicted_keys.len() + 1);
            for key in &evicted_keys {
                requests.push(scope.queue_delete_row(key)?);
            }
            if self.byte_cap.is_some() {
                requests.push(
                    scope.queue_write_used_bytes(used_bytes.saturating_sub(reclaimed_bytes))?,
                );
            }
            request::requests(&requests).await?;
        }
        scope.done().await
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
        let mut reclaimed_bytes = 0_u64;
        for key in keys {
            let stored = scope.row(&key).await?;
            let retired_bytes = match js_value::deserialize::<Option<StoredRow>>(stored)? {
                Some(row) => {
                    let row_bytes = row.accounted_bytes();
                    if self.decode_under_authority::<V>(row.data)?.is_none() {
                        Some(row_bytes)
                    } else {
                        None
                    }
                }
                None => None,
            };
            if let Some(row_bytes) = retired_bytes {
                self.authority.report_retired(&key);
                scope.delete_row(&key).await?;
                reclaimed_bytes = reclaimed_bytes.saturating_add(row_bytes);
            }
        }
        if self.byte_cap.is_some() && reclaimed_bytes > 0 {
            let used = scope.used_bytes_record().await?.unwrap_or(0);
            request::request(&scope.queue_write_used_bytes(used.saturating_sub(reclaimed_bytes))?)
                .await?;
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
        // Reading is a recency touch: use `Durability::Default` so `DurableCache` reads do not
        // trigger synchronous OS fsyncs merely to advance `access_stamp`.
        let scope = self
            .scope_with_durability(IdbTransactionMode::Readwrite, request::Durability::Default)?;
        let stored = scope.row(key).await?;
        let Some(row) = js_value::deserialize::<Option<StoredRow>>(stored)? else {
            scope.done().await?;
            return Ok(None);
        };
        let row_bytes = row.accounted_bytes();
        let row_byte_len = row.byte_len;
        // Decode before scheduling any write, so a kept row stays untouched.
        let value = match self.decode_under_authority(row.data.clone())? {
            Some(value) => value,
            None => {
                self.authority.report_retired(&row.key);
                scope.delete_row(&row.key).await?;
                if self.byte_cap.is_some() && row_bytes > 0 {
                    let used = scope.used_bytes_record().await?.unwrap_or(0);
                    request::request(
                        &scope.queue_write_used_bytes(used.saturating_sub(row_bytes))?,
                    )
                    .await?;
                }
                scope.done().await?;
                return Ok(None);
            }
        };
        scope
            .put_stamped(row.key, row_byte_len, row.data, None)
            .await?;
        scope.done().await?;
        Ok(Some(value))
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        // Serialize and validate capacity before opening a destructive transaction so a failure
        // cannot evict any previously stored row.
        let data = js_value::serialize_storage(value)?;
        let byte_len = match self.byte_cap {
            Some(byte_cap) => {
                let len = measure_wire_bytes(key, value)?;
                if len > byte_cap {
                    return Err(Error::StorageValueExceedsCapacity {
                        required: u64::from(len),
                        capacity: u64::from(byte_cap),
                    });
                }
                Some(len)
            }
            None => None,
        };
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let existing_raw = scope.row(key).await?;
        let existing_row = if existing_raw.is_undefined() || existing_raw.is_null() {
            None
        } else {
            js_value::deserialize::<Option<StoredRow>>(existing_raw)?
        };
        let incoming_rows = u32::from(existing_row.is_none());
        let existing_bytes = existing_row.as_ref().map_or(0, StoredRow::accounted_bytes);
        let incoming_bytes = u64::from(byte_len.unwrap_or(0));

        let excess_rows = if incoming_rows > 0 {
            let count = scope.count().await?;
            if count.saturating_add(incoming_rows) > self.cap && !self.authority.evicts() {
                return Err(Error::StorageBudgetExhausted {
                    used: u64::from(count),
                    required: u64::from(incoming_rows),
                    capacity: u64::from(self.cap),
                });
            }
            count.saturating_add(incoming_rows).saturating_sub(self.cap)
        } else {
            0
        };

        let (used_bytes_before, excess_bytes) = match self.byte_cap {
            Some(byte_cap) => {
                let used = scope.used_bytes_record().await?.unwrap_or(0);
                let base = used.saturating_sub(existing_bytes);
                let projected = base.saturating_add(incoming_bytes);
                let cap_u64 = u64::from(byte_cap);
                if projected > cap_u64 && !self.authority.evicts() {
                    return Err(Error::StorageBudgetExhausted {
                        used: base,
                        required: incoming_bytes,
                        capacity: cap_u64,
                    });
                }
                (Some(base), projected.saturating_sub(cap_u64))
            }
            None => (None, 0),
        };

        let reclaimed_bytes = if excess_rows > 0 || excess_bytes > 0 {
            let mut excluded = std::collections::BTreeSet::new();
            if existing_row.is_some() {
                excluded.insert(key);
            }
            let (evicted_keys, reclaimed) = scope
                .least_recent_rows_for_budget(&excluded, excess_rows, excess_bytes)
                .await?;
            let requests = evicted_keys
                .iter()
                .map(|evicted| scope.queue_delete_row(evicted))
                .collect::<Result<Vec<_>>>()?;
            request::requests(&requests).await?;
            reclaimed
        } else {
            0
        };

        let next_used_bytes = used_bytes_before.map(|base| {
            base.saturating_sub(reclaimed_bytes)
                .saturating_add(incoming_bytes)
        });
        scope
            .put_stamped(key.to_owned(), byte_len, data, next_used_bytes)
            .await?;
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
        if self.byte_cap.is_some() {
            let existing_raw = scope.row(key).await?;
            let existing_bytes = if existing_raw.is_undefined() || existing_raw.is_null() {
                0
            } else {
                js_value::deserialize::<Option<StoredRow>>(existing_raw)?
                    .map_or(0, |row| row.accounted_bytes())
            };
            let used = scope.used_bytes_record().await?.unwrap_or(0);
            let delete_req = scope.queue_delete_row(key)?;
            let bytes_req = scope.queue_write_used_bytes(used.saturating_sub(existing_bytes))?;
            request::requests(&[delete_req, bytes_req]).await?;
        } else {
            scope.delete_row(key).await?;
        }
        scope.done().await
    }

    /// Delete all values. The clock keeps running, so later stamps stay above earlier ones.
    async fn clear(&self) -> Result<()> {
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let clear_req = scope.rows.clear().map_err(request::js_error)?;
        if self.byte_cap.is_some() {
            let bytes_req = scope.queue_write_used_bytes(0)?;
            request::requests(&[clear_req, bytes_req]).await?;
        } else {
            request::request(&clear_req).await?;
        }
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

    /// Every row in one read-write transaction, committed with this store's durability (one
    /// strict commit for the whole batch): all or nothing. Every value is serialized and all
    /// reads and budget checks complete before any write or delete request is queued in a single
    /// synchronous turn, so a refusal or mid-flight cancellation leaves no partial write.
    async fn put_batch(&self, records: &[(String, V)]) -> Result<()> {
        let mut deduplicated = super::write_ordered::WriteOrderedMap::default();
        for (key, value) in records {
            deduplicated.insert(key.to_owned(), value);
        }
        if deduplicated.len() == 0 {
            return Ok(());
        }
        let batch_len =
            u32::try_from(deduplicated.len()).map_err(|_| Error::StorageCountOverflow)?;
        if batch_len > self.cap && self.authority.evicts() {
            return Err(Error::StorageValueExceedsCapacity {
                required: u64::from(batch_len),
                capacity: u64::from(self.cap),
            });
        }
        let mut serialized = Vec::with_capacity(deduplicated.len());
        let mut batch_bytes = 0_u64;
        while let Some((key, value)) = deduplicated.pop_oldest() {
            let data = js_value::serialize_storage(value)?;
            let byte_len = match self.byte_cap {
                Some(_) => {
                    let len = measure_wire_bytes(&key, value)?;
                    batch_bytes = batch_bytes.saturating_add(u64::from(len));
                    Some(len)
                }
                None => None,
            };
            serialized.push((key, byte_len, data));
        }
        if let Some(byte_cap) = self.byte_cap {
            if batch_bytes > u64::from(byte_cap) && self.authority.evicts() {
                return Err(Error::StorageValueExceedsCapacity {
                    required: batch_bytes,
                    capacity: u64::from(byte_cap),
                });
            }
        }
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let mut rows = Vec::with_capacity(serialized.len());
        let mut incoming = 0_u32;
        let mut overwritten_bytes = 0_u64;
        for (key, byte_len, data) in serialized {
            let existing_raw = scope.row(&key).await?;
            let existing_row = if existing_raw.is_undefined() || existing_raw.is_null() {
                None
            } else {
                js_value::deserialize::<Option<StoredRow>>(existing_raw)?
            };
            let is_existing = existing_row.is_some();
            if let Some(existing_row) = existing_row {
                overwritten_bytes =
                    overwritten_bytes.saturating_add(existing_row.accounted_bytes());
            } else {
                incoming = incoming.saturating_add(1);
            }
            rows.push((key, is_existing, byte_len, data));
        }
        let count = if incoming > 0 || batch_len > self.cap {
            let count = scope.count().await?;
            if (count.saturating_add(incoming) > self.cap || batch_len > self.cap)
                && !self.authority.evicts()
            {
                return Err(Error::StorageBudgetExhausted {
                    used: u64::from(count),
                    required: u64::from(incoming),
                    capacity: u64::from(self.cap),
                });
            }
            count
        } else {
            0
        };
        let excess_rows = if incoming > 0 {
            count.saturating_add(incoming).saturating_sub(self.cap)
        } else {
            0
        };
        let (used_bytes_base, excess_bytes) = match self.byte_cap {
            Some(byte_cap) => {
                let used = scope.used_bytes_record().await?.unwrap_or(0);
                let base = used.saturating_sub(overwritten_bytes);
                let projected = base.saturating_add(batch_bytes);
                let cap_u64 = u64::from(byte_cap);
                if projected > cap_u64 && !self.authority.evicts() {
                    return Err(Error::StorageBudgetExhausted {
                        used: base,
                        required: batch_bytes,
                        capacity: cap_u64,
                    });
                }
                (Some(base), projected.saturating_sub(cap_u64))
            }
            None => (None, 0),
        };
        let (evicted_keys, reclaimed_bytes) = if excess_rows > 0 || excess_bytes > 0 {
            let existing_keys = rows
                .iter()
                .filter_map(|(key, is_existing, _, _)| is_existing.then_some(key.as_str()))
                .collect::<std::collections::BTreeSet<_>>();
            scope
                .least_recent_rows_for_budget(&existing_keys, excess_rows, excess_bytes)
                .await?
        } else {
            (Vec::new(), 0)
        };
        let mut clock = scope
            .clock_record()
            .await?
            .ok_or(Error::IdbAccessClockMissing)?;
        let mut encoded_rows = Vec::with_capacity(rows.len());
        for (key, _, byte_len, data) in rows {
            let (access_stamp, successor) = clock.tick()?;
            clock = successor;
            encoded_rows.push(js_value::serialize_storage(&StoredRow {
                key,
                access_stamp,
                byte_len,
                data,
            })?);
        }
        let successor = js_value::serialize_storage(&clock)?;
        let mut write_requests = Vec::with_capacity(evicted_keys.len() + encoded_rows.len() + 2);
        for key in &evicted_keys {
            write_requests.push(scope.queue_delete_row(key)?);
        }
        for row in &encoded_rows {
            write_requests.push(scope.queue_put_row(row)?);
        }
        write_requests.push(scope.queue_write_clock(&successor)?);
        if let Some(base) = used_bytes_base {
            write_requests.push(
                scope.queue_write_used_bytes(
                    base.saturating_sub(reclaimed_bytes)
                        .saturating_add(batch_bytes),
                )?,
            );
        }
        request::requests(&write_requests).await?;
        scope.done().await
    }

    /// Every row deleted in one read-write transaction, committed with this store's durability:
    /// all or nothing.
    async fn remove_batch(&self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let scope = self.scope(IdbTransactionMode::Readwrite)?;
        let mut unique_keys = std::collections::BTreeSet::new();
        for key in keys {
            unique_keys.insert(key.as_str());
        }
        let mut reclaimed_bytes = 0_u64;
        if self.byte_cap.is_some() {
            for key in &unique_keys {
                let existing_raw = scope.row(key).await?;
                if !(existing_raw.is_undefined() || existing_raw.is_null()) {
                    if let Some(row) = js_value::deserialize::<Option<StoredRow>>(existing_raw)? {
                        reclaimed_bytes = reclaimed_bytes.saturating_add(row.accounted_bytes());
                    }
                }
            }
        }
        let mut requests = Vec::with_capacity(unique_keys.len() + 1);
        for key in unique_keys {
            requests.push(scope.queue_delete_row(key)?);
        }
        if self.byte_cap.is_some() {
            let used = scope.used_bytes_record().await?.unwrap_or(0);
            requests.push(scope.queue_write_used_bytes(used.saturating_sub(reclaimed_bytes))?);
        }
        request::requests(&requests).await?;
        scope.done().await
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
            .field("byte_cap", &self.byte_cap)
            .field("authority", &self.authority)
            .finish()
    }
}

#[cfg(test)]
#[path = "idb/tests.rs"]
mod tests;
