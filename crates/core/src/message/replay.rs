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
//! No frame is held on the way: a frame whose delegation reference misses is dropped, never
//! delivered late, and an honest sender's reference resolves whenever its transit stays under
//! the lease margin and grace (the lease law of `swarm::session_link`, #908), so the law holds
//! for every frame that arrives.
//!
//! Persistence is one record per stream (see [`store`]): a transition writes only its stream's
//! record, at most [`TRANSACTION_REPLAY_RECORD_MAX_BYTES`], so the cost of an admission does not
//! grow with the number of streams retained.
//!
//! An admission writes its stream's record every time; a reservation writes it only when it
//! outruns the stream's durable lease of sender sequences, which then covers the next
//! `TRANSACTION_SEQUENCE_LEASE` sequences (#1043). A restart resumes past the lease: it may
//! skip sequences, never reuse one (the laws of [`lease`]). A link-local transaction (#1081) meets
//! none of this: it is admitted by [`TransactionReplay::admit_on_link`] in a volatile window of
//! the link it arrived on, which its connection holds ([`LinkWindows`]), and writes nothing
//! (durable-records, Law Window by classification). The records of the transitions that
//! run while one batch is written share the next batch, one flush for all of them, and each
//! transition is released only once its batch is durable (the group-commit law of [`commit`],
//! #916).
//!
//! **Law (fail closed per stream, #910).** The first operation restores the store from one scan
//! of the storage and caches the result, including every record that does not restore. For a
//! stream `s` whose stored record is damaged (torn, corrupt or unreadable, or holding another
//! key's record) or missing (witnessed as written by the store, and no longer held, #915):
//!
//! ```text
//! ∀ transition t of s.   t = Err(TransactionReplayStreamUnavailable { s, record })
//!                        until the record is cleared and the node restarts
//! ∀ s′ ≠ s restored.     state(s′) and the verdicts of s′ are those of a store without s
//! ∀ s′ new.              s′ opens only if its table's streams + |U| < capacity
//! cost(restore) = O(|store|), once;  cost(call) = 0 store reads + at most 1 record in a batch
//! ```
//!
//! so no replay is admitted from, and no sequence is reused by, such a stream, while every stream
//! already in the store keeps its guarantee; a new stream sees each such record hold one slot of
//! its table's bound. The law covers the records the storage witnesses: the native store keeps a
//! presence marker per record, so a record deleted or renamed outside it is missing, not a
//! stream never seen; only a store lost whole (its directory, or the browser's database) reads
//! as a stream never seen, for every stream, which is the reset below. Each batch of the native
//! store is flushed, which bounds the rate of batches, not of transitions (see the replay
//! chapter). The unrestorable
//! records are counted ([`ReplayCounters::unrestorable_record`]), each refusal is counted
//! ([`ReplayCounters::unavailable_stream`]), and each is logged once at load with its storage
//! record name. A scan that fails as a whole restores nothing: it is counted as a persistence
//! failure, and the next operation scans again.
//!
//! **Recovery.** An operator clears one failed stream, accepting a replay-window reset for that
//! stream alone, by removing the record the refusal and the log name while the node is stopped,
//! and then starting it: the native daemon keeps each record as the file of that name in the
//! `transaction-replay` directory beside its data store, with its presence marker `<name>.p`
//! beside it, which the operator removes too; a browser provider keeps it as the
//! row of that key in the IndexedDB database and object store `<storage name>/transaction-replay`
//! (see the replay chapter for the steps). After the restart the stream starts from `First`: an
//! unexpired transaction of a cleared receiver stream may be admitted once more, and a cleared
//! sender stream restarts at sequence zero. Its destination rejects those sequences until they
//! pass its retained high watermark: as `Stale` below the window, and as `Fork`, with signed
//! [`TransactionForkEvidence`] against this node, inside it; the messages they carry are lost.
//! No other stream is touched.
//!
//! **Law (stream admission, #907; load-bounds paper, Law 2).** The receiver table is a table
//! under slot accounting (`fair_admission::slot_accounting`): a new stream is charged to the
//! connection slot of the edge that delivered its first transaction, or to this node for its own
//! relay inbox and
//! signaling, and admitted only within that principal's floor, fair claim of the pool, or local
//! share ([`admission::RECEIVER_STREAM_SHARES`]); an origin, which costs nothing to mint, is never
//! a principal. A stream is spent once its every transaction can no longer be delivered
//! ([`spent_at`]):
//!
//! ```text
//! spent(s, t) ⟺ t ≥ max_{x ∈ s} (ts(x) + ttl(x)) + ι(class(s)) + σ
//! ```
//!
//! and only a spent stream that holds no relay custody leaves the table, retired by the next
//! admission that reads the clock past it; forgetting it reopens no replay (Lemma 2), so a
//! refusal lasts until a stream of the refused principal is spent, never for good. Each
//! receiver record carries its stream's charge, so a restart recovers the accounting.
//!
//! The sender table holds this node's own streams: it never evicts, and a new stream fails
//! closed at [`TRANSACTION_REPLAY_STREAM_CAPACITY`], which also bounds the receiver table. The
//! store must therefore hold [`TRANSACTION_REPLAY_STORE_MAX_BYTES`] and
//! [`TRANSACTION_REPLAY_STORE_MAX_RECORDS`] without evicting any. Deleting that store deletes
//! the corresponding replay guarantee. Runtime-local origin quota state shares the serialized
//! receiver commit boundary but is not part of the store.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::lock::Mutex;
use rings_runtime::MaybeSend;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::Principal;
use crate::fair_admission::SlotFloorUse;
use crate::fair_admission::TableAccounting;
use crate::message::quota::quota_admission_error;
use crate::message::quota::OriginQuotaCounterState;
use crate::message::quota::OriginQuotaTable;
use crate::message::types::MessageCategory;
use crate::message::OriginQuotaCharge;
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaCounters;
use crate::message::OriginQuotaKey;
use crate::storage::KvStorageScan;
use crate::swarm::transport::DELIVERY_BOUND;
use crate::utils::Instant;

mod admission;
mod commit;
mod custody;
mod lease;
mod store;

use self::admission::replay_clock;
pub(crate) use self::admission::spent_at;
pub(crate) use self::admission::Admission;
use self::admission::ReplayInstant;
pub(crate) use self::admission::RECEIVER_STREAM_SHARES;
use self::commit::Journal;
use self::commit::Ledger;
pub(crate) use self::custody::RelayAdmission;
pub(crate) use self::custody::RelayDelivery;
use self::lease::allocate;
use self::store::restore;
use self::store::sender_record;
pub use self::store::ReplayRecord;
use self::store::ReplayStore;
use self::store::ReplayTable;
use self::store::RestoreFailure;
use self::store::StreamCharge;
pub use self::store::TRANSACTION_REPLAY_RECORD_MAX_BYTES;
pub use self::store::TRANSACTION_REPLAY_STORE_MAX_BYTES;
pub use self::store::TRANSACTION_REPLAY_STORE_MAX_RECORDS;

/// Number of out-of-order sequence slots retained for one destination-scoped stream.
pub const TRANSACTION_REPLAY_WINDOW: usize = 32;
/// How many later frames of one stream may overtake a frame that still arrives: the replay
/// window, which refuses a frame further behind. A stream consumer reorders at most this far.
pub const REORDER_WINDOW: u64 = 32;
const _: () = assert!(REORDER_WINDOW == TRANSACTION_REPLAY_WINDOW as u64);
/// How long a frame of one stream may still arrive after a later frame of that stream did.
///
/// On one edge a stream's frames arrive in order (the in-class order law). End to end they may
/// not, when the stream's route, or the connection generation of one of its hops, changes
/// mid-stream: frames still in flight on the superseded path land later than frames sent on
/// the new one, each within the delivery bound of every hop it has yet to cross, since every
/// hop delivers a frame or gives it up within its delivery bound. The deadline allows four such
/// bounds: a superseded path with at most four hops left to cross. A stream consumer takes a
/// frame still missing this long after a later one arrived as lost; one that has seen
/// [`REORDER_WINDOW`] later frames of it has lost it, since the replay window refuses it.
pub const REORDER_DEADLINE: Duration = DELIVERY_BOUND.saturating_mul(4);
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
/// and the one data channel it is pinned to delivers them in that order.
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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
        if gap >= REORDER_WINDOW {
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

/// `storage`'s record naming, under which it files each record and reports the records it cannot
/// decode.
fn record_naming(storage: &ReplayStorage) -> impl Fn(&str) -> String + '_ {
    |storage_key| storage.record_name(storage_key)
}

/// The volatile replay windows of one link (#1081): the window of each stream whose
/// link-local transactions arrived on it. Held by the link's inbound stage, never written, and
/// released with the link, so a window lives exactly one link generation (durable-records, Law
/// Window by classification).
pub(crate) type LinkWindows = BTreeMap<StreamKey, SequenceState>;

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
/// One mutex serializes every reservation and admission on the speculative tables, and the
/// records are written by group commit outside it (the group-commit law of [`commit`], #916):
/// the transitions that run while one batch is written join the next, which one store batch
/// writes with one flush, and each transition is released only once its batch is durable. Each
/// record is at most [`TRANSACTION_REPLAY_RECORD_MAX_BYTES`].
///
/// **Law (whole transitions).** A transition, once started, runs to its end: lock, apply its
/// effect, join the open batch, unlock, and wait for the batch. [`Self::reserve`] and
/// [`Self::admit_with_quota`] hand it to the runtime ([`rings_runtime::run_detached`]), so
/// cancelling the caller abandons only the wait. Otherwise a cancelled caller could leave the
/// speculative tables holding an effect its batch never carries, and the next transition of the
/// stream would compute from it: a receiver replay, or re-signed sender sequences, after a
/// restart.
///
/// A cancelled caller's transition therefore commits, exactly as if the caller had dropped the
/// verdict: an admission enters the window and is charged its quota, though nobody dispatches
/// it, so a retransmission of that transaction is a `Replay` and the message is lost (as for
/// any drop after admission); a reservation consumes its sequences, which leaves a gap the
/// receiver accepts as `Advance`. Cancellation also no longer sheds a queued transition: each
/// cancelled attempt still joins its batch.
///
/// ```text
/// caller ──call──▶ run_detached ─▶ [lock ─▶ T̂ := t(T̂) ─▶ join batch ─▶ unlock] ─▶ durable
///   │ drop                                   (owned by the runtime, never torn)
///   └──────────▶ only the wait is abandoned
/// ```
pub(crate) struct TransactionReplay {
    /// The replay store; `None` when the node was given none, so that every transition is
    /// refused (`Error::TransactionReplayStorageUnavailable`, #1022): durable-records admits no
    /// volatile replay store, whose loss at every restart would reopen every stream.
    storage: Option<ReplayStorage>,
    /// The receiver table's slot accounting (load-bounds paper, Law 2).
    accounting: TableAccounting,
    state: Mutex<TransactionAdmissionState>,
    started_at: Instant,
    counters: ReplayCounterState,
    quota_counters: OriginQuotaCounterState,
}

struct TransactionAdmissionState {
    /// The replay store, restored once on the first operation and cached from then on; its
    /// tables are the speculative ones (the group-commit law).
    store: Option<ReplayStore>,
    /// Runtime-local origin quotas.
    quota: OriginQuotaTable,
    /// The open batch and the undo log of the one being written.
    journal: Journal,
}

impl TransactionReplay {
    /// Construct a shared replay runtime, as the transport holds it. The store is restored
    /// lazily on its first operation.
    #[cfg(test)]
    pub(crate) fn new_shared(storage: ReplayStorage) -> Arc<Self> {
        Arc::new(Self::with_quota_for_test(
            Some(storage),
            OriginQuotaConfig::default(),
        ))
    }

    /// Test builds: a replay runtime over `storage` with `quota_config` and the production
    /// shares.
    #[cfg(test)]
    pub(crate) fn with_quota_for_test(
        storage: Option<ReplayStorage>,
        quota_config: OriginQuotaConfig,
    ) -> Self {
        let accounting = TableAccounting::new(RECEIVER_STREAM_SHARES);
        Self::new_with_quota(storage, quota_config, accounting)
    }

    /// Construct a replay runtime over `storage` with explicit runtime-local origin quotas,
    /// whose receiver table is accounted in `accounting`; with no storage, every reservation and
    /// admission is refused (the field's law).
    pub(crate) fn new_with_quota(
        storage: Option<ReplayStorage>,
        quota_config: OriginQuotaConfig,
        accounting: TableAccounting,
    ) -> Self {
        Self {
            storage,
            accounting,
            state: Mutex::new(TransactionAdmissionState {
                store: None,
                quota: OriginQuotaTable::new(quota_config),
                journal: Journal::default(),
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

    /// The floor use of every slot in the receiver table at `now_ms` (load-bounds paper,
    /// Def. 1).
    pub(crate) fn floor_use_at(&self, now_ms: u128) -> SlotFloorUse {
        self.accounting.floor_use_at(now_ms)
    }

    /// Current aggregate origin-quota rejection counters.
    pub(crate) fn quota_counters(&self) -> OriginQuotaCounters {
        self.quota_counters.snapshot()
    }

    /// The replay store, restored from one scan of the storage on the first operation and
    /// cached from then on, unrestorable records included, so no later operation reads the
    /// storage again.
    ///
    /// A scan that fails as a whole restores nothing: it is counted, the operation fails closed,
    /// and the next operation scans again.
    ///
    /// ```text
    /// slot cached? ── yes ──────────────────────────────────────────────▶ slot
    ///      │ no
    /// no storage ──────────▶ Err(TransactionReplayStorageUnavailable)
    /// scan storage ── Err ──▶ count persistence failure ──▶ Err(load) (slot stays empty)
    ///      │ Ok(records)
    /// restore (pure, total) ──▶ count + log unrestorable ──▶ charge streams ──▶ slot := store
    /// ```
    async fn load_store<'a>(
        &self,
        slot: &'a mut Option<ReplayStore>,
    ) -> Result<&'a mut ReplayStore> {
        if slot.is_none() {
            let storage = self.storage()?;
            let records = storage.scan().await.map_err(|source| {
                self.count_persistence_failure();
                Error::TransactionReplayPersistence {
                    operation: "load",
                    source: Arc::new(source),
                }
            })?;
            let restored = restore(records, record_naming(storage));
            self.report_unrestorable(&restored.unrestorable);
            restored.tables.charge_all(&mut self.accounting.lock());
            *slot = Some(restored);
        }
        slot.as_mut().ok_or(Error::TransactionReplayStateInvalid)
    }

    /// The replay store, or the refusal of a node given none (the law of `storage`).
    fn storage(&self) -> Result<&ReplayStorage> {
        self.storage
            .as_ref()
            .ok_or(Error::TransactionReplayStorageUnavailable)
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
        match store.unavailable_record(table, &key, record_naming(self.storage()?))? {
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

    /// Reserve `count` sender sequences, staging the stream's lease whenever they outrun it,
    /// and return the range to the signer once its batch is durable (the laws of [`lease`] and
    /// [`commit`]).
    ///
    /// The transition runs detached from the caller (the law of whole transitions): cancelling
    /// the caller abandons only the wait.
    pub(crate) async fn reserve(
        self: &Arc<Self>,
        key: StreamKey,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        self.commit(move |replay, ledger| replay.allocate_in(ledger, key, count))
            .await
    }

    /// The reservation transition on the speculative state: allocate, stage the extended lease
    /// if the allocation outruns the stream's lease, then update the table.
    ///
    /// ```text
    /// allocate(table[key], count) ── ⊥ ──▶ Err(TransactionSequenceExhausted)
    ///      │ extend = None ───────────────────────────▶ table[key] := next ──▶ range
    ///      │ extend = Some(L) ──▶ stage D := L ───────▶ table[key] := next ──▶ range
    /// ```
    fn allocate_in(
        &self,
        ledger: &mut Ledger<'_>,
        key: StreamKey,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        self.refuse_unavailable(ledger.store, ReplayTable::Sender, key)?;
        let previous = ledger.store.tables.sender.get(&key).copied();
        if previous.is_none() {
            ledger
                .store
                .admits_new_stream(ReplayTable::Sender)
                .map_err(|refusal| Error::TransactionReplayStreamCapacityExceeded {
                    introducer: Principal::Local,
                    refusal,
                })?;
        }
        let allocation =
            allocate(previous, count).ok_or(Error::TransactionSequenceExhausted { key })?;
        if let Some(leased) = allocation.extend {
            ledger.write(sender_record(&key, leased)?);
        }
        ledger.set_sender(key, allocation.next);
        Ok(allocation.range)
    }

    /// Atomically commit replay classification and origin-quota admission before dispatch.
    ///
    /// The transition runs detached from the caller (the law of whole transitions): cancelling
    /// the caller abandons only the wait.
    pub(crate) async fn admit_with_quota(
        self: &Arc<Self>,
        admission: Admission,
    ) -> Result<SequenceVerdict> {
        let clock = replay_clock(self.started_at);
        self.admit_at(admission, clock).await
    }

    /// The admission transition: read the clock, retire the spent streams, classify, reserve
    /// quota, update the table and stage the stream's record, as one transition (the
    /// group-commit law).
    ///
    /// Law (Clock under the lock): `clock` is read only inside the critical section of
    /// `state`, so the instants the quota table receives follow the order in which
    /// transitions hold the lock. A monotonic clock then gives every quota record a
    /// non-decreasing sequence of instants, and `MonotonicTimeRegressed` is unreachable;
    /// an instant read before the lock could be overtaken by a later reading that locked
    /// first (#953).
    async fn admit_at<C>(
        self: &Arc<Self>,
        admission: Admission,
        clock: C,
    ) -> Result<SequenceVerdict>
    where
        C: FnOnce() -> ReplayInstant + rings_runtime::MaybeSend + 'static,
    {
        self.commit(move |replay, ledger| replay.admit_in(ledger, admission, clock, None))
            .await
    }

    /// The admission transition on the speculative state (load-bounds paper, Law 2). A
    /// `relayed` delivery enters the stream's custody in the same record (the custody law of
    /// `custody`); a live one leaves the custody as it is.
    ///
    /// ```text
    /// now := clock();  retire every stream spent at now.wall
    /// s ∉ table ── table full ─────────────────────▶ Err(StreamCapacityExceeded { λ/π, TableFull })
    ///          └── accounting refuses π ───────────▶ Err(StreamCapacityExceeded { π, refusal })
    /// observe(window(s)) ── Replay/Fork/Stale ─────▶ Err(…)
    /// quota(origin) ── refused ────────────────────▶ Err(quota)
    /// window(s) := next;  custody(s) += relayed;  charge(s) := (π(s), max(spent_at))
    /// stage record(s) ──▶ Ok(verdict)
    /// ```
    fn admit_in<C>(
        &self,
        ledger: &mut Ledger<'_>,
        admission: Admission,
        clock: C,
        relayed: Option<RelayDelivery>,
    ) -> Result<SequenceVerdict>
    where
        C: FnOnce() -> ReplayInstant,
    {
        let Admission {
            key,
            sequence,
            digest,
            charge,
            byte_cost,
            introducer,
            spent_at_ms,
        } = admission;
        let now = clock();
        ledger.retire_spent(now.wall_ms)?;
        self.refuse_unavailable(ledger.store, ReplayTable::Receiver, key)?;
        let previous = ledger.store.tables.receiver.get(&key);
        if previous.is_none() {
            ledger
                .store
                .admits_new_stream(ReplayTable::Receiver)
                .and_then(|()| ledger.accounting.lock().admit(introducer))
                .map_err(|refusal| Error::TransactionReplayStreamCapacityExceeded {
                    introducer,
                    refusal,
                })?;
        }
        let (next, verdict) = observe(previous.cloned(), sequence, digest);
        self.refuse_rejected(key, sequence, verdict)?;

        let quota_key = OriginQuotaKey::new(
            key.network_id,
            key.origin_account,
            key.destination,
            charge.lane,
        );
        let reservation =
            ledger
                .quota
                .reserve(quota_key, charge.message_limit, byte_cost, now.quota);
        match reservation {
            Ok(reservation) => ledger.charged(reservation),
            Err(error) => {
                self.quota_counters.record(charge.lane, &error);
                return Err(quota_admission_error(quota_key, byte_cost, error));
            }
        }
        ledger.set_receiver(key, next);
        if relayed.is_some() {
            let mut custody = ledger
                .store
                .tables
                .custody
                .get(&key)
                .cloned()
                .unwrap_or_default();
            custody.extend(relayed);
            ledger.set_custody(key, custody);
        }
        let held = ledger.store.tables.charge(&key);
        let stream_charge = StreamCharge {
            introducer: held.map_or(introducer, |held| held.introducer),
            spent_at_ms: held.map_or(spent_at_ms, |held| held.spent_at_ms.max(spent_at_ms)),
        };
        if held != Some(stream_charge) {
            ledger.set_charge(key, stream_charge);
        }
        ledger.write(ledger.store.tables.receiver_record(&key)?);
        Ok(verdict)
    }

    /// Refuse, and count, a verdict that does not permit dispatch: `Replay`, `Fork` (with its
    /// evidence) or `Stale`, whichever window observed it.
    fn refuse_rejected(
        &self,
        key: StreamKey,
        sequence: u64,
        verdict: SequenceVerdict,
    ) -> Result<()> {
        match verdict {
            SequenceVerdict::First | SequenceVerdict::Advance | SequenceVerdict::Late => Ok(()),
            SequenceVerdict::Replay => {
                self.counters.replay.fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionReplay { key, sequence })
            }
            SequenceVerdict::Fork { accepted, incoming } => {
                self.counters.fork.fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionSequenceFork {
                    key,
                    sequence,
                    evidence: Box::new(TransactionForkEvidence { accepted, incoming }),
                })
            }
            SequenceVerdict::Stale { retained_min } => {
                self.counters.stale.fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionSequenceStale {
                    key,
                    sequence,
                    retained_min,
                })
            }
        }
    }

    /// Admit a link-local transaction of stream `key` in `window`, the volatile window of that
    /// stream on the link it arrived on (#1081; durable-records, Law Window by classification):
    /// observe, read the quota clock and reserve quota under the lock, then advance the window.
    ///
    /// Nothing is written, and the verdict is returned at once: it rests on the link's volatile
    /// window and the runtime-local quotas, never on a durable state. The quota reservation is
    /// logged into the pending batch, if any, so a failed batch still rewinds the quota table
    /// last first (the fail-closed law of [`commit`]); the admission itself stands. A refusal
    /// leaves `window` and the quotas unchanged.
    pub(crate) async fn admit_on_link(
        &self,
        window: &mut Option<SequenceState>,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
        charge: OriginQuotaCharge,
        byte_cost: usize,
    ) -> Result<SequenceVerdict> {
        let clock = replay_clock(self.started_at);
        let mut state = self.state.lock().await;
        let TransactionAdmissionState { quota, journal, .. } = &mut *state;
        let (next, verdict) = observe(window.clone(), sequence, digest);
        self.refuse_rejected(key, sequence, verdict)?;
        let quota_key = OriginQuotaKey::new(
            key.network_id,
            key.origin_account,
            key.destination,
            charge.lane,
        );
        let reservation = quota
            .reserve(quota_key, charge.message_limit, byte_cost, clock().quota)
            .map_err(|error| {
                self.quota_counters.record(charge.lane, &error);
                quota_admission_error(quota_key, byte_cost, error)
            })?;
        journal.log_released_charge(reservation);
        *window = Some(next);
        Ok(verdict)
    }

    /// Test builds: admit `sequence` and `digest` of stream `key`, introduced by this node,
    /// at quota instant zero and at the wall clock now.
    #[cfg(test)]
    async fn admit(
        self: &Arc<Self>,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
    ) -> Result<SequenceVerdict> {
        let now_ms = crate::utils::get_epoch_ms();
        let admission = Admission::for_test(key, sequence, digest, now_ms);
        self.admit_at(admission, move || ReplayInstant {
            quota: crate::message::OriginQuotaInstant::ZERO,
            wall_ms: now_ms,
        })
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
mod test_custody;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_durable_throughput;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_group_commit;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_lease;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_load_failures;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_slot_accounting;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_storage;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_stream_failures;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_whole_transitions;
