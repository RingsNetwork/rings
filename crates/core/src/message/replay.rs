//! Destination-scoped replay protection for signed transactions.
//!
//! The stream identity is `(network_id, origin account DID, destination DID, traffic class)`
//! (#898). A receiver keeps a fixed-width window per stream, so delivery may be reordered within
//! the window without turning a normal gap into an omission claim. The state transition is pure
//! in [`observe`]; [`TransactionReplay`] is the effect boundary that serializes transitions and
//! persists an admitted state before returning it to the inbound dispatcher.
//!
//! **Law (in-class order).** The sender signs a class's transactions in sequence order and its
//! outbound scheduler keeps them in that order on the class lane, with fewer than
//! [`TRANSACTION_REPLAY_WINDOW`] in flight. Each class lane is pinned to one ordered data channel
//! of the connection, so a class's stream reaches this admission in sequence order whatever the
//! other classes' channels do, and a stall on one class's channel holds only that class. An
//! honest stream crossing one edge is therefore never rejected as stale; a class lane that is
//! not pinned (frames spread over several channels) loses this law, since another channel can
//! carry later sequences of the class past a stalled one.
//!
//! The law covers the frames that resolve on arrival. A frame whose delegation reference misses
//! (the receiver forgot the delegation to capacity eviction or expiry) is held on the session
//! link for one repair round trip and released independently of the frames behind it, since
//! the link promises no order among held frames (see the delegation-references chapter of the
//! book, `docs/src/advanced-topic/delegation-references.md`). Later frames of its class may
//! reach this admission first; once a window's worth have, the released frame is rejected as
//! stale. That is the loss of one frame, counted, never a false admission; removing it is
//! tracked in #908.
//!
//! Persistence is one record per stream (see [`store`]): a transition writes only its stream's
//! record, at most [`TRANSACTION_REPLAY_RECORD_MAX_BYTES`], so the cost of an admission does not
//! grow with the number of streams retained.
//!
//! **Law (fail closed per stream, #910).** The first operation restores the store from one scan
//! of the storage and caches the result, including every record that does not restore. For a
//! stream `s` whose stored record is damaged (torn, corrupt or unreadable, or holding another
//! key's record):
//!
//! ```text
//! ∀ transition t of s.   t = Err(TransactionReplayStreamUnavailable { s, record })
//!                        until the record is cleared and the node restarts
//! ∀ s′ ≠ s restored.     state(s′) and the verdicts of s′ are those of a store without s
//! ∀ s′ new.              s′ opens iff its table's streams + |U| < capacity
//! cost(restore) = O(|store|), once;  cost(call) = 0 store reads + 1 record write
//! ```
//!
//! so no replay is admitted from, and no sequence is reused by, such a stream, while every stream
//! already in the store keeps its guarantee; a new stream sees each such record hold one slot of
//! its table's bound. The law covers the records the storage holds: a record that is absent
//! (deleted, or never made durable, as a rename can be on a non-unix target) is indistinguishable
//! from a stream never seen, and its stream restarts from `First` (tracked in #915). Each record
//! write of the native store is flushed, which bounds the node-wide transition rate (about 50 per
//! second on macOS, see the replay chapter; group commit is tracked in #916). The unrestorable
//! records are counted ([`ReplayCounters::unrestorable_record`]), each refusal is counted
//! ([`ReplayCounters::unavailable_stream`]), and each is logged once at load with its storage
//! record name. A scan that fails as a whole restores nothing: it is counted as a persistence
//! failure, and the next operation scans again.
//!
//! **Recovery.** An operator clears one failed stream, accepting a replay-window reset for that
//! stream alone, by removing the record the refusal and the log name while the node is stopped,
//! and then starting it: the native daemon keeps each record as the file of that name in the
//! `transaction-replay` directory beside its data store, and a browser provider keeps it as the
//! row of that key in the IndexedDB database and object store `<storage name>/transaction-replay`
//! (see the replay chapter for the steps). After the restart the stream starts from `First`: an
//! unexpired transaction of a cleared receiver stream may be admitted once more, and a cleared
//! sender stream restarts at sequence zero. Its destination rejects those sequences until they
//! pass its retained high watermark: as `Stale` below the window, and as `Fork`, with signed
//! [`TransactionForkEvidence`] against this node, inside it; the messages they carry are lost.
//! No other stream is touched.
//!
//! The first load that finds the shared-stream
//! snapshot of the key used before #898 deletes it without reading it (best effort, counted on
//! failure); once it is gone no later load touches it. Sender and receiver tables each have a
//! hard stream-count bound and never evict: once the bound is reached, a new stream fails
//! closed. Existing stream records remain durable until an operator explicitly removes the
//! replay store, which must therefore hold [`TRANSACTION_REPLAY_STORE_MAX_BYTES`] and
//! [`TRANSACTION_REPLAY_STORE_MAX_RECORDS`] without evicting any. Deleting that store deletes
//! the corresponding replay guarantee. Runtime-local origin quota state shares the serialized
//! receiver commit boundary but is not part of the store.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::lock::Mutex;
use rings_runtime::MaybeSend;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::quota::quota_admission_error;
use crate::message::quota::OriginQuotaCounterState;
use crate::message::quota::OriginQuotaTable;
use crate::message::types::MessageCategory;
use crate::message::OriginQuotaCharge;
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaCounters;
use crate::message::OriginQuotaInstant;
use crate::message::OriginQuotaKey;
use crate::storage::KvStorageScan;
use crate::utils::Instant;

mod store;

use self::store::receiver_record;
use self::store::restore;
use self::store::sender_record;
pub use self::store::ReplayRecord;
use self::store::ReplayStore;
use self::store::ReplayTable;
use self::store::RestoreFailure;
use self::store::SHARED_STREAM_SNAPSHOT_KEY;
pub use self::store::TRANSACTION_REPLAY_RECORD_MAX_BYTES;
pub use self::store::TRANSACTION_REPLAY_STORE_MAX_BYTES;
pub use self::store::TRANSACTION_REPLAY_STORE_MAX_RECORDS;

/// Number of out-of-order sequence slots retained for one destination-scoped stream.
pub const TRANSACTION_REPLAY_WINDOW: usize = 32;
const TRANSACTION_REPLAY_WINDOW_U64: u64 = 32;
const TRANSACTION_REPLAY_BACKTRACK: u64 = 31;
/// Account-to-destination pairs whose four class streams one runtime retains in full.
const TRANSACTION_REPLAY_PAIR_CAPACITY: usize = 4096;
/// Maximum sender streams and maximum receiver streams retained by one runtime.
///
/// A pair keeps one stream per traffic class (#898), so the table holds one stream per
/// [`MessageCategory`] for each of 4096 pairs: 4096 origins that use every class, or
/// proportionally more that use fewer. A pair's streams count separately, and a new stream
/// fails closed at this bound whichever class it belongs to.
pub const TRANSACTION_REPLAY_STREAM_CAPACITY: usize =
    MessageCategory::COUNT * TRANSACTION_REPLAY_PAIR_CAPACITY;
/// A destination-scoped transaction stream of one traffic class (#898).
///
/// Sequences are ordered only per class. The outbound scheduler preserves order inside a
/// class lane and deliberately reorders across lanes, so a stream shared by every class let a
/// backlog in one lane fall behind later-sequenced traffic of another lane and be rejected as
/// stale. With one stream per class, an honest sender's transactions reach the receiver in
/// sequence order: the class lane keeps them FIFO with fewer in flight than the replay window,
/// and the one data channel it is pinned to delivers them in that order (a frame held on a
/// delegation-reference miss excepted, #908; see the module documentation).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct StreamKey {
    /// Overlay in which the transaction signature is valid.
    pub network_id: u32,
    /// Account DID recovered from the transaction's delegation.
    pub origin_account: Did,
    /// Final logical destination of the transaction.
    pub destination: Did,
    /// Traffic class implied by the transaction's signed data.
    pub class: MessageCategory,
}

impl StreamKey {
    /// Name one account-to-destination stream of `class` inside an overlay.
    pub const fn new(
        network_id: u32,
        origin_account: Did,
        destination: Did,
        class: MessageCategory,
    ) -> Self {
        Self {
            network_id,
            origin_account,
            destination,
            class,
        }
    }
}

/// Digest of one exact signed transaction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransactionDigest(pub(crate) [u8; 32]);

impl TransactionDigest {
    /// Construct a digest from its canonical 32-byte representation.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the canonical digest bytes.
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// The two exact signed-transaction digests retained for a sequence fork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionForkEvidence {
    /// Digest already retained for the sequence.
    pub accepted: TransactionDigest,
    /// Digest presented by the conflicting transaction.
    pub incoming: TransactionDigest,
}

/// Fixed-width accepted-sequence window for one stream.
///
/// `accepted.last()` is sequence `high`; preceding slots descend toward
/// `high.saturating_sub(31)`. Slots below sequence zero are necessarily empty.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SequenceState {
    high: u64,
    accepted: [Option<TransactionDigest>; TRANSACTION_REPLAY_WINDOW],
}

impl SequenceState {
    fn first(sequence: u64, digest: TransactionDigest) -> Self {
        let mut accepted = [None; TRANSACTION_REPLAY_WINDOW];
        if let Some(slot) = accepted.last_mut() {
            *slot = Some(digest);
        }
        Self {
            high: sequence,
            accepted,
        }
    }

    /// Highest sequence observed for this stream.
    pub const fn high(&self) -> u64 {
        self.high
    }

    /// Lowest sequence whose digest may still be retained.
    pub const fn retained_min(&self) -> u64 {
        self.high.saturating_sub(TRANSACTION_REPLAY_BACKTRACK)
    }

    fn is_valid(&self) -> bool {
        let retained_slots = self
            .high
            .checked_add(1)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(TRANSACTION_REPLAY_WINDOW)
            .min(TRANSACTION_REPLAY_WINDOW);
        let unused_slots = TRANSACTION_REPLAY_WINDOW.saturating_sub(retained_slots);
        self.accepted.iter().take(unused_slots).all(Option::is_none)
            && self.accepted.last().is_some_and(Option::is_some)
    }

    fn advance(&mut self, sequence: u64, digest: TransactionDigest) {
        let gap = sequence.saturating_sub(self.high);
        if gap >= TRANSACTION_REPLAY_WINDOW_U64 {
            self.accepted.fill(None);
        } else if let Ok(gap) = usize::try_from(gap) {
            self.accepted.rotate_left(gap);
            let retained = TRANSACTION_REPLAY_WINDOW.saturating_sub(gap);
            for slot in self.accepted.iter_mut().skip(retained) {
                *slot = None;
            }
        }
        self.high = sequence;
        if let Some(slot) = self.accepted.last_mut() {
            *slot = Some(digest);
        }
    }

    fn observe_retained(&mut self, sequence: u64, digest: TransactionDigest) -> SequenceVerdict {
        let distance = self.high.saturating_sub(sequence);
        let Ok(distance) = usize::try_from(distance) else {
            return SequenceVerdict::Stale {
                retained_min: self.retained_min(),
            };
        };
        let Some(index) = TRANSACTION_REPLAY_WINDOW.checked_sub(distance.saturating_add(1)) else {
            return SequenceVerdict::Stale {
                retained_min: self.retained_min(),
            };
        };
        let Some(slot) = self.accepted.get_mut(index) else {
            return SequenceVerdict::Stale {
                retained_min: self.retained_min(),
            };
        };
        match *slot {
            None => {
                *slot = Some(digest);
                SequenceVerdict::Late
            }
            Some(accepted) if accepted == digest => SequenceVerdict::Replay,
            Some(accepted) => SequenceVerdict::Fork {
                accepted,
                incoming: digest,
            },
        }
    }
}

/// Typed result of observing one sequence and exact signed-transaction digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequenceVerdict {
    /// No retained state existed; this transaction established the stream baseline.
    First,
    /// The sequence advanced the high watermark, possibly across a normal delivery gap.
    Advance,
    /// The sequence filled an empty slot inside the retained reordering window.
    Late,
    /// The same exact signed transaction was already retained at this sequence.
    Replay,
    /// A different signed transaction was already retained at this sequence.
    Fork {
        /// Digest retained for the sequence.
        accepted: TransactionDigest,
        /// Digest presented by the new transaction.
        incoming: TransactionDigest,
    },
    /// The sequence fell below the retained window.
    Stale {
        /// Lowest sequence that may still be retained.
        retained_min: u64,
    },
}

impl SequenceVerdict {
    /// Whether this observation may cross the application dispatch boundary.
    pub const fn permits_dispatch(self) -> bool {
        matches!(self, Self::First | Self::Advance | Self::Late)
    }
}

/// Apply the pure replay-window transition.
///
/// Transition shape: `Option<SequenceState> -> (sequence, digest) -> (SequenceState, verdict)`.
/// Rejected observations return the original state unchanged.
pub fn observe(
    state: Option<SequenceState>,
    sequence: u64,
    digest: TransactionDigest,
) -> (SequenceState, SequenceVerdict) {
    let Some(mut state) = state else {
        return (
            SequenceState::first(sequence, digest),
            SequenceVerdict::First,
        );
    };
    if sequence > state.high {
        state.advance(sequence, digest);
        return (state, SequenceVerdict::Advance);
    }
    if sequence < state.retained_min() {
        let retained_min = state.retained_min();
        return (state, SequenceVerdict::Stale { retained_min });
    }
    let verdict = state.observe_retained(sequence, digest);
    (state, verdict)
}

/// Run one replay transition detached from its caller (the law of whole transitions of
/// [`TransactionReplay`]).
///
/// Post: `Err(TransactionReplayUnscheduled)` when no runtime is current (the transition never
/// started) or the transition panicked; otherwise the transition's own result.
async fn run_whole<T, F>(transition: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + MaybeSend + 'static,
    T: MaybeSend + 'static,
{
    rings_runtime::run_detached(transition)
        .await
        .map_err(Error::TransactionReplayUnscheduled)?
}

/// Storage accepted by the transaction replay runtime.
pub type ReplayStorage = Box<rings_runtime::maybe_send_sync!(dyn KvStorageScan<ReplayRecord>)>;

/// Observable rejected-verdict, persistence-failure and unavailable-stream counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayCounters {
    /// Exact duplicate transactions rejected.
    pub replay: u64,
    /// Conflicting transactions at one sequence rejected.
    pub fork: u64,
    /// Transactions below the retained window rejected.
    pub stale: u64,
    /// Replay store reads or writes that failed.
    pub persistence_failure: u64,
    /// Records the load found that do not restore, each failing its stream closed; a gauge,
    /// fixed once the store is loaded, and non-zero calls for an operator.
    pub unrestorable_record: u64,
    /// Reservations and admissions refused because their stream's record does not restore.
    pub unavailable_stream: u64,
}

/// The atomic cells behind [`ReplayCounters`].
#[derive(Default)]
struct ReplayCounterState {
    replay: AtomicU64,
    fork: AtomicU64,
    stale: AtomicU64,
    persistence_failure: AtomicU64,
    unrestorable_record: AtomicU64,
    unavailable_stream: AtomicU64,
}

impl ReplayCounterState {
    /// The current value of every counter.
    fn snapshot(&self) -> ReplayCounters {
        ReplayCounters {
            replay: self.replay.load(Ordering::Relaxed),
            fork: self.fork.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            persistence_failure: self.persistence_failure.load(Ordering::Relaxed),
            unrestorable_record: self.unrestorable_record.load(Ordering::Relaxed),
            unavailable_stream: self.unavailable_stream.load(Ordering::Relaxed),
        }
    }
}

/// Serialized sender allocator and receiver replay-window effect boundary.
///
/// One mutex serializes every reservation and admission, and it is held across the store write
/// of the stream's record, so the replay store's write latency bounds the rate of replay
/// transitions node-wide. Each write is one record of at most
/// [`TRANSACTION_REPLAY_RECORD_MAX_BYTES`]; a lock per stream would let writes of different
/// streams overlap.
///
/// **Law (whole transitions).** A transition, once started, runs to its end: lock, persist the
/// stream's record, update the in-memory table, unlock. [`Self::reserve`] and
/// [`Self::admit_with_quota`] hand it to the runtime ([`rings_runtime::run_detached`]), so
/// cancelling the caller abandons only the wait. Otherwise a cancelled caller would release the
/// mutex while its record write still ran, the next transition of the stream would compute from
/// a table without that write, and the stale write could land last on disk: a receiver replay,
/// or re-signed sender sequences, after a restart.
///
/// A cancelled caller's transition therefore commits, exactly as if the caller had dropped the
/// verdict: an admission enters the window and is charged its quota, though nobody dispatches
/// it, so a retransmission of that transaction is a `Replay` and the message is lost (as for
/// any drop after admission); a reservation consumes its sequences, which leaves a gap the
/// receiver accepts as `Advance`. Cancellation also no longer sheds a queued transition: each
/// cancelled attempt still waits its turn and pays its flushed write.
///
/// ```text
/// caller ──call──▶ run_detached ─▶ [lock ─▶ persist ─▶ table := next ─▶ unlock]
///   │ drop                                   (owned by the runtime, never torn)
///   └──────────▶ only the wait is abandoned
/// ```
pub(crate) struct TransactionReplay {
    storage: ReplayStorage,
    state: Mutex<TransactionAdmissionState>,
    started_at: Instant,
    counters: ReplayCounterState,
    quota_counters: OriginQuotaCounterState,
}

struct TransactionAdmissionState {
    /// The replay store, restored once on the first operation and cached from then on.
    store: Option<ReplayStore>,
    /// Runtime-local origin quotas.
    quota: OriginQuotaTable,
}

impl TransactionReplay {
    /// Construct a shared replay runtime, as the transport holds it. The store is restored
    /// lazily on its first operation.
    #[cfg(test)]
    pub(crate) fn new_shared(storage: ReplayStorage) -> Arc<Self> {
        Arc::new(Self::new_with_quota(storage, OriginQuotaConfig::default()))
    }

    /// Construct a replay runtime with explicit runtime-local origin quotas.
    pub(crate) fn new_with_quota(storage: ReplayStorage, quota_config: OriginQuotaConfig) -> Self {
        Self {
            storage,
            state: Mutex::new(TransactionAdmissionState {
                store: None,
                quota: OriginQuotaTable::new(quota_config),
            }),
            started_at: Instant::now(),
            counters: ReplayCounterState::default(),
            quota_counters: OriginQuotaCounterState::default(),
        }
    }

    /// Current observable counters.
    pub(crate) fn counters(&self) -> ReplayCounters {
        self.counters.snapshot()
    }

    /// Current aggregate origin-quota rejection counters.
    pub(crate) fn quota_counters(&self) -> OriginQuotaCounters {
        self.quota_counters.snapshot()
    }

    /// The replay store, restored from one scan of the storage on the first operation and
    /// cached from then on, unrestorable records included, so no later operation reads the
    /// storage again.
    ///
    /// A load that finds the shared-stream snapshot of the key used before #898 retires it, so
    /// once its deletion succeeds no later load, in this run or after a restart, touches it. A
    /// scan that fails as a whole restores nothing: it is counted, the operation fails closed,
    /// and the next operation scans again.
    ///
    /// ```text
    /// slot cached? ── yes ──────────────────────────────────────────────▶ slot
    ///      │ no
    /// scan storage ── Err ──▶ count persistence failure ──▶ Err(load) (slot stays empty)
    ///      │ Ok(records)
    /// restore (pure, total) ──▶ count + log unrestorable ──▶ retire shared snapshot?
    ///      └────────────────────────────────▶ slot := store ──▶ slot
    /// ```
    async fn load_store<'a>(
        &self,
        slot: &'a mut Option<ReplayStore>,
    ) -> Result<&'a mut ReplayStore> {
        if slot.is_none() {
            let records = self.storage.scan().await.map_err(|source| {
                self.count_persistence_failure();
                Error::TransactionReplayPersistence {
                    operation: "load",
                    source: Box::new(source),
                }
            })?;
            let restored = restore(records, self.record_naming());
            self.report_unrestorable(&restored.store.unrestorable);
            if restored.holds_shared_snapshot {
                self.retire_shared_stream_snapshot().await;
            }
            *slot = Some(restored.store);
        }
        slot.as_mut().ok_or(Error::TransactionReplayStateInvalid)
    }

    /// The storage's record naming, under which it files each record and reports the records it
    /// cannot decode.
    fn record_naming(&self) -> impl Fn(&str) -> String + '_ {
        |storage_key| self.storage.record_name(storage_key)
    }

    /// Count and log the `unrestorable` records of a fresh load; each fails its stream closed
    /// until an operator clears it (see the module documentation).
    fn report_unrestorable(&self, unrestorable: &BTreeMap<String, RestoreFailure>) {
        let count = u64::try_from(unrestorable.len()).unwrap_or(u64::MAX);
        self.counters
            .unrestorable_record
            .store(count, Ordering::Relaxed);
        for (record, failure) in unrestorable.iter() {
            tracing::error!(
                record = %record,
                failure = ?failure,
                "replay record does not restore; its stream fails closed until it is cleared"
            );
        }
    }

    /// Refuse, and count, a transition of stream `key` of `table` whose record does not
    /// restore (the fail-closed-per-stream law of [`store`]).
    fn refuse_unavailable(
        &self,
        store: &ReplayStore,
        table: ReplayTable,
        key: StreamKey,
    ) -> Result<()> {
        match store.unavailable_record(table, &key, self.record_naming())? {
            None => Ok(()),
            Some(record) => {
                self.counters
                    .unavailable_stream
                    .fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionReplayStreamUnavailable { key, record })
            }
        }
    }

    /// Count one failed replay-store read or write.
    fn count_persistence_failure(&self) {
        self.counters
            .persistence_failure
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Delete the shared-stream snapshot of the key used before #898, without reading it.
    ///
    /// Best effort: the former key is never decoded, so a failed deletion leaves only inert
    /// bytes. It is counted as a persistence failure and logged, admission continues, and the
    /// next load retries it.
    async fn retire_shared_stream_snapshot(&self) {
        if let Err(error) = self.storage.remove(SHARED_STREAM_SNAPSHOT_KEY).await {
            self.count_persistence_failure();
            tracing::warn!(
                %error,
                key = SHARED_STREAM_SNAPSHOT_KEY,
                "failed to delete the shared-stream replay snapshot; it is never read"
            );
        }
    }

    /// Store one stream's `record` under `storage_key`; a failure is counted and returned.
    async fn persist(&self, (storage_key, record): (String, ReplayRecord)) -> Result<()> {
        self.storage
            .put(storage_key.as_str(), &record)
            .await
            .map_err(|source| {
                self.count_persistence_failure();
                Error::TransactionReplayPersistence {
                    operation: "store",
                    source: Box::new(source),
                }
            })
    }

    /// Reserve and persist `count` sender sequences before returning the range to the signer.
    ///
    /// The transition runs detached from the caller (the law of whole transitions): cancelling
    /// the caller abandons only the wait.
    pub(crate) async fn reserve(
        self: &Arc<Self>,
        key: StreamKey,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        let replay = Arc::clone(self);
        run_whole(async move { replay.commit_reservation(key, count).await }).await
    }

    /// The reservation transition: lock, load, reserve, persist the stream's record, then update
    /// the table.
    async fn commit_reservation(
        &self,
        key: StreamKey,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        let mut state = self.state.lock().await;
        let store = self.load_store(&mut state.store).await?;
        self.refuse_unavailable(store, ReplayTable::Sender, key)?;
        let previous = store.tables.sender.get(&key).copied();
        if previous.is_none() && !store.admits_new_stream(ReplayTable::Sender) {
            return Err(Error::TransactionReplayStreamCapacityExceeded {
                capacity: TRANSACTION_REPLAY_STREAM_CAPACITY,
            });
        }
        let first = match previous {
            Some(last) => last
                .checked_add(1)
                .ok_or(Error::TransactionSequenceExhausted { key })?,
            None => 0,
        };
        let last = first
            .checked_add(count.get().saturating_sub(1))
            .ok_or(Error::TransactionSequenceExhausted { key })?;
        self.persist(sender_record(&key, last)?).await?;
        store.tables.sender.insert(key, last);
        Ok(first..=last)
    }

    /// Atomically commit replay classification and origin-quota admission before dispatch.
    ///
    /// The transition runs detached from the caller (the law of whole transitions): cancelling
    /// the caller abandons only the wait.
    pub(crate) async fn admit_with_quota(
        self: &Arc<Self>,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
        charge: OriginQuotaCharge,
        byte_cost: usize,
    ) -> Result<SequenceVerdict> {
        let now = OriginQuotaInstant::from_nanos(
            Instant::now()
                .saturating_duration_since(self.started_at)
                .as_nanos(),
        );
        let replay = Arc::clone(self);
        run_whole(async move {
            replay
                .admit_at(key, sequence, digest, charge, byte_cost, now)
                .await
        })
        .await
    }

    /// The admission transition at quota time `now`: lock, load, classify, reserve quota,
    /// persist the stream's record, then update the table.
    async fn admit_at(
        &self,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
        charge: OriginQuotaCharge,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<SequenceVerdict> {
        let mut state = self.state.lock().await;
        let TransactionAdmissionState { store, quota } = &mut *state;
        let store = self.load_store(store).await?;
        self.refuse_unavailable(store, ReplayTable::Receiver, key)?;
        let previous = store.tables.receiver.get(&key);
        if previous.is_none() && !store.admits_new_stream(ReplayTable::Receiver) {
            return Err(Error::TransactionReplayStreamCapacityExceeded {
                capacity: TRANSACTION_REPLAY_STREAM_CAPACITY,
            });
        }
        let (next, verdict) = observe(previous.cloned(), sequence, digest);
        match verdict {
            SequenceVerdict::First | SequenceVerdict::Advance | SequenceVerdict::Late => {}
            SequenceVerdict::Replay => {
                self.counters.replay.fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionReplay { key, sequence });
            }
            SequenceVerdict::Fork { accepted, incoming } => {
                self.counters.fork.fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionSequenceFork {
                    key,
                    sequence,
                    evidence: Box::new(TransactionForkEvidence { accepted, incoming }),
                });
            }
            SequenceVerdict::Stale { retained_min } => {
                self.counters.stale.fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionSequenceStale {
                    key,
                    sequence,
                    retained_min,
                });
            }
        }

        let record = receiver_record(&key, &next)?;
        let quota_key = OriginQuotaKey::new(
            key.network_id,
            key.origin_account,
            key.destination,
            charge.lane,
        );
        let reservation = quota.reserve(quota_key, charge.message_limit, byte_cost, now);
        let quota_reservation = match reservation {
            Ok(reservation) => reservation,
            Err(error) => {
                self.quota_counters.record(charge.lane, &error);
                return Err(quota_admission_error(quota_key, byte_cost, error));
            }
        };
        if let Err(error) = self.persist(record).await {
            quota_reservation.rollback(quota);
            return Err(error);
        }
        store.tables.receiver.insert(key, next);
        Ok(verdict)
    }

    #[cfg(test)]
    async fn admit(
        &self,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
    ) -> Result<SequenceVerdict> {
        self.admit_at(
            key,
            sequence,
            digest,
            crate::message::MessageCategory::Application.into(),
            0,
            OriginQuotaInstant::ZERO,
        )
        .await
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) async fn quota_record_count_for_test(&self) -> usize {
        self.state.lock().await.quota.len()
    }

    /// The whole `(message, byte)` tokens `key`'s quota record held after its last admission.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn quota_tokens_for_test(&self, key: OriginQuotaKey) -> Option<(u128, u128)> {
        self.state.lock().await.quota.whole_tokens(key)
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, not(target_family = "wasm")))]
mod quota_admission_tests;
#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
mod test_browser_store;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_durable_throughput;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_load_failures;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_storage;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_stream_failures;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_whole_transitions;
