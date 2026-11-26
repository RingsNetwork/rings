//! Runtime adapter for the pure `rings-measure` state relation.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::FutureExt;
use futures::StreamExt;
use rings_core::dht::Did;
use rings_core::measure;
use rings_core::measure::Measure;
use rings_core::measure::PeerMeasurement;
use rings_core::measure::PeerMeasurementPage;
use rings_core::measure::PeerQuality;
use rings_core::measure::PeerQualityThresholds;
use rings_core::storage::KvStorageInterface;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;
use rings_measure::ApplyOutcome;
use rings_measure::Authentication;
use rings_measure::CreditPolicy;
use rings_measure::EvidenceAdmission;
use rings_measure::EvidenceAdmissionDelta;
use rings_measure::EvidenceAdmissionReport;
use rings_measure::EvidenceCounters;
use rings_measure::EvidenceDigest;
use rings_measure::EvidenceError;
use rings_measure::EvidenceLimits;
use rings_measure::EvidenceLoadReport;
use rings_measure::EvidencePage;
use rings_measure::EvidenceReplayMarker;
use rings_measure::EvidenceSnapshot;
use rings_measure::EvidenceStorageEntry;
use rings_measure::MeasureError;
use rings_measure::MeasurementBatch;
use rings_measure::MeasurementEvent;
use rings_measure::MeasurementLedger;
use rings_measure::MeasurementSnapshot;
use rings_measure::ProvisionalEvidenceRecord;
use rings_measure::ProvisionalEvidenceStore;
use rings_measure::ReliabilityPolicy;
use rings_measure::UnixTime;
use rings_measure::EVIDENCE_SNAPSHOT_VERSION;
use rings_runtime::sleep;
use rings_runtime::RuntimeUnavailable;
use rings_runtime::Spawner;
use rings_runtime::TaskSet;
use rings_runtime::TimerError;

// Legacy `PeriodicMeasure/counters/...` values intentionally remain unread:
// a bare count proves neither byte-credit direction nor a live epoch timestamp.
const SNAPSHOT_KEY: &str = "MeasurementLedger";
const EVIDENCE_SNAPSHOT_KEY: &str = "ProvisionalEvidence";
/// Prefix for sequentially numbered incremental evidence log entries.
const EVIDENCE_DELTA_KEY_PREFIX: &str = "ProvisionalEvidenceDelta/";
/// Maximum uncompacted incremental evidence entries retained before snapshot compaction.
const EVIDENCE_DELTA_COMPACTION_THRESHOLD: usize = 512;

/// Format the deterministic storage key for incremental evidence entry `sequence`.
fn evidence_delta_key(sequence: u64) -> String {
    format!("{EVIDENCE_DELTA_KEY_PREFIX}{sequence:020}")
}
const PERSISTENCE_WAKE_CAPACITY: usize = 1;
const PERSISTENCE_SHUTDOWN_ATTEMPTS: usize = 3;
const PRUNE_INTERVAL_SECONDS: u64 = 60 * 60;
#[cfg(test)]
const PERSISTENCE_MIN_INTERVAL: Duration = Duration::from_millis(50);
// Measurement-ledger mutations are coalesced over this interval. Provisional
// evidence admission uses the same serialization lock but commits separately
// before returning success.
#[cfg(not(test))]
const PERSISTENCE_MIN_INTERVAL: Duration = Duration::from_secs(60);
// One window for tests and production. Controlled-clock unit tests advance the clock by
// `RELIABILITY_WINDOW` to drive epoch rollover deterministically, so they need no shorter
// window. A sub-second test window instead broke wall-clock integration tests: reliability
// evidence is windowed (reset when a fresh observation lands in a later epoch) while credit
// is cumulative, so under live keepalive traffic an epoch boundary between a send and a
// later read could reset `evidence.sent` to 0 even though credit had grown — a flaky
// `evidence.sent >= 1`. A production-sized window is never crossed within a test.
#[allow(
    clippy::unwrap_used,
    reason = "the non-zero integer literal is validated during const evaluation"
)]
const RELIABILITY_WINDOW: NonZeroU64 = NonZeroU64::new(3_600).unwrap();

/// Shared peer-quality thresholds used by measurement and route selection.
pub(crate) const fn peer_quality_thresholds() -> PeerQualityThresholds {
    PeerQualityThresholds::new(
        crate::consts::CONNECT_FAILED_LIMIT,
        crate::consts::MSG_SEND_FAILED_LIMIT,
        crate::consts::MSG_RECV_FAILED_LIMIT,
    )
}

const fn reliability_policy() -> ReliabilityPolicy {
    ReliabilityPolicy::from_nonzero_window(RELIABILITY_WINDOW, 1, peer_quality_thresholds())
}

/// Storage used for one versioned complete measurement snapshot.
pub type MeasureStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<MeasurementSnapshot<Did>>)>;

/// Storage used for the separate provisional-receipt evidence log and checkpoints.
pub type EvidenceStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<EvidenceStorageEntry<Did>>)>;

/// Evidence backend used when a provider did not configure durable receipt storage.
///
/// Reads expose an empty initial state so measurement-only runtimes can start, but every write
/// fails. Consequently the admission adapter rolls back and never returns `Admitted` under a
/// process-local store that cannot refine the crash-recovery model.
pub(crate) struct UnavailableEvidenceStorage;

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl KvStorageInterface<EvidenceStorageEntry<Did>> for UnavailableEvidenceStorage {
    async fn get(
        &self,
        _key: &str,
    ) -> rings_core::error::Result<Option<EvidenceStorageEntry<Did>>> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &str,
        value: &EvidenceStorageEntry<Did>,
    ) -> rings_core::error::Result<()> {
        if let EvidenceStorageEntry::Snapshot {
            compacted_through_sequence: 0,
            snapshot,
        } = value
        {
            if snapshot.records.is_empty()
                && snapshot.replay_markers.is_empty()
                && snapshot.replay_floor == 0
                && snapshot.counters == EvidenceCounters::default()
            {
                return Ok(());
            }
        }
        Err(EvidenceError::StorageUnavailable.into())
    }

    async fn get_all(&self) -> rings_core::error::Result<Vec<(String, EvidenceStorageEntry<Did>)>> {
        Ok(Vec::new())
    }

    async fn remove(&self, _key: &str) -> rings_core::error::Result<()> {
        Ok(())
    }

    async fn clear(&self) -> rings_core::error::Result<()> {
        Ok(())
    }

    async fn count(&self) -> rings_core::error::Result<u32> {
        Ok(0)
    }
}

/// Runtime identity that owns one persisted provisional-evidence collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvidenceCollectorIdentity {
    network_id: u32,
    provider_account: Did,
}

impl EvidenceCollectorIdentity {
    /// Bind persisted evidence to one overlay and the local provider account.
    pub const fn new(network_id: u32, provider_account: Did) -> Self {
        Self {
            network_id,
            provider_account,
        }
    }
}

type SharedMeasureStorage =
    Arc<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<MeasurementSnapshot<Did>>)>;

type SharedEvidenceStorage =
    Arc<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<EvidenceStorageEntry<Did>>)>;

/// Failure while loading or explicitly flushing the runtime measurement adapter.
#[derive(Debug, thiserror::Error)]
pub enum MeasureRuntimeError {
    /// The configured key-value backend failed.
    #[error("measurement storage failed: {0}")]
    Storage(#[from] rings_core::error::Error),
    /// Persisted or live state violated the pure measurement model.
    #[error("measurement model failed: {0}")]
    Model(#[from] MeasureError),
    /// Persisted or live provisional evidence violated its bounded model.
    #[error("provisional evidence model failed: {0}")]
    Evidence(#[from] EvidenceError),
    /// A bounded explicit flush did not complete before its deadline.
    #[error("measurement persistence flush timed out")]
    FlushTimeout,
    /// The runtime stopped the owned flush task before it returned a result.
    #[error("measurement persistence flush task stopped")]
    FlushTaskStopped,
    /// The browser could not schedule a measurement timer.
    #[error("measurement timer failed: {0}")]
    Timer(#[from] TimerError),
    /// Native construction was attempted without a live Tokio runtime.
    #[error("measurement persistence requires a live runtime: {0}")]
    RuntimeUnavailable(#[from] RuntimeUnavailable),
}

/// Pure-ledger runtime adapter with durable evidence admission and coalesced measurement snapshots.
///
/// Measurement callbacks update in-memory state and replace the pending full
/// snapshot. Receipt admission commits its evidence snapshot before success.
/// One runtime lock serializes both write paths. The algorithm, time projection,
/// pruning, and snapshot schemas remain in `rings-measure`.
pub struct PeriodicMeasure {
    state: Arc<MeasureState>,
    persistence_wake: mpsc::Sender<()>,
    /// Resolves once the state, with its stores, is dropped (see `MeasureState::_released`).
    released: oneshot::Receiver<Infallible>,
}

struct MeasureState {
    storage: SharedMeasureStorage,
    evidence_storage: SharedEvidenceStorage,
    /// Uncompacted delta count that triggers background snapshot compaction.
    evidence_compaction_threshold: usize,
    /// Synchronous in-memory ledger and evidence state (`runtime` is a leaf never held across
    /// `.await`).
    runtime: Mutex<RuntimeLedger>,
    /// Cancel-safe FIFO turn serializer for durable ledger snapshot writes (`storage`).
    ledger_turns: WriteTurnQueue,
    /// Cancel-safe FIFO turn serializer for durable evidence writes (`evidence_storage`).
    evidence_turns: WriteTurnQueue,
    /// Waiters coalescing behind the active or next explicit [`PeriodicMeasure::flush`] pass.
    pending_flushes: Mutex<Vec<oneshot::Sender<Result<(), MeasureRuntimeError>>>>,
    clock: Arc<dyn MeasureClock>,
    /// The executor captured at construction, which owns persistence and flush work.
    spawner: Spawner,
    /// The measurement's own tasks, its persistence worker and flushes, spawned on `spawner`.
    /// Not the node's tasks: they run until the measurement's owner drops it, after the node's
    /// tasks have drained, and a close awaits them through its stores' release (#1085).
    tasks: TaskSet,
    /// Never sent on, only dropped: fields drop in declaration order, so this one drops after
    /// both stores, and its receiver's `Canceled` is the event that the stores are released.
    _released: oneshot::Sender<Infallible>,
}

/// Cancel-safe FIFO turn serializer for async storage writes without holding an async mutex lock
/// across I/O or triggering slow-async-lock warnings.
#[derive(Default)]
struct WriteTurnQueue {
    state: Mutex<WriteTurnState>,
}

#[derive(Default)]
struct WriteTurnState {
    active: bool,
    next_id: u64,
    waiters: VecDeque<(u64, oneshot::Sender<()>)>,
}

struct WriteTurnGuard<'a> {
    queue: &'a WriteTurnQueue,
}

impl Drop for WriteTurnGuard<'_> {
    fn drop(&mut self) {
        self.queue.release_turn();
    }
}

struct PendingTurn<'a> {
    queue: &'a WriteTurnQueue,
    id: u64,
    rx: oneshot::Receiver<()>,
    acquired: bool,
}

impl Drop for PendingTurn<'_> {
    fn drop(&mut self) {
        if self.acquired {
            return;
        }
        let removed = {
            let mut state = lock_or_recover(&self.queue.state);
            let before = state.waiters.len();
            state.waiters.retain(|(id, _)| *id != self.id);
            state.waiters.len() < before
        };
        if removed {
            return;
        }
        self.rx.close();
        if matches!(self.rx.try_recv(), Ok(Some(()))) {
            self.queue.release_turn();
        }
    }
}

impl WriteTurnQueue {
    async fn acquire(&self) -> WriteTurnGuard<'_> {
        loop {
            let mut pending = {
                let mut state = lock_or_recover(&self.state);
                if !state.active {
                    state.active = true;
                    return WriteTurnGuard { queue: self };
                }
                let id = state.next_id;
                state.next_id = state.next_id.wrapping_add(1);
                let (tx, rx) = oneshot::channel();
                state.waiters.push_back((id, tx));
                PendingTurn {
                    queue: self,
                    id,
                    rx,
                    acquired: false,
                }
            };
            if (&mut pending.rx).await.is_ok() {
                pending.acquired = true;
                return WriteTurnGuard { queue: self };
            }
            pending.acquired = true;
        }
    }

    fn release_turn(&self) {
        loop {
            let waiter = {
                let mut state = lock_or_recover(&self.state);
                match state.waiters.pop_front() {
                    Some((_, waiter)) => waiter,
                    None => {
                        state.active = false;
                        return;
                    }
                }
            };
            if waiter.send(()).is_ok() {
                return;
            }
        }
    }
}

impl MeasureState {
    /// Serialize clock sampling with the ledger transition it timestamps.
    fn runtime_at_now(&self) -> (std::sync::MutexGuard<'_, RuntimeLedger>, UnixTime) {
        let runtime = lock_or_recover(&self.runtime);
        let now = self.clock.now();
        (runtime, now)
    }
}

struct RuntimeLedger {
    ledger: MeasurementLedger<Did>,
    evidence: ProvisionalEvidenceStore<Did>,
    /// Last durable view while an admission write is in flight, so read-only evidence queries
    /// need only the synchronous `runtime` lock without observing uncommitted state.
    committed_evidence_override: Option<Arc<ProvisionalEvidenceStore<Did>>>,
    /// Next monotonic sequence number for an incremental provisional-evidence entry.
    next_evidence_delta_seq: u64,
    /// Uncompacted `(sequence, key)` entries currently persisted in `evidence_storage`.
    pending_evidence_deltas: Vec<(u64, String)>,
    dirty: bool,
    persisting: bool,
    mutated_while_persisting: bool,
    evidence_dirty: bool,
    evidence_persisting: bool,
    evidence_mutated_while_persisting: bool,
    last_clock: UnixTime,
    next_prune_at: UnixTime,
}

impl RuntimeLedger {
    fn committed_evidence(&self) -> &ProvisionalEvidenceStore<Did> {
        self.committed_evidence_override
            .as_deref()
            .unwrap_or(&self.evidence)
    }
}

// Boundary: the adapter supplies wall-clock seconds to the pure state relation.
trait MeasureClock: Send + Sync {
    fn now(&self) -> UnixTime;
}

struct SharedMeasureClock(SharedClock);

impl MeasureClock for SharedMeasureClock {
    fn now(&self) -> UnixTime {
        let seconds = u64::try_from(self.0.now_ms() / 1_000).unwrap_or(u64::MAX);
        UnixTime::from_secs(seconds)
    }
}

impl PeriodicMeasure {
    /// Load the complete ledger once and start the coalescing persistence task.
    ///
    /// On native targets this captures the active Tokio runtime handle. The
    /// constructor returns `MeasureRuntimeError::RuntimeUnavailable` instead
    /// of panicking when called outside a live runtime.
    /// Provisional receipt admission is disabled; use [`Self::new_with_evidence_storage`]
    /// when successful receipt admission must survive process restart.
    pub async fn new(storage: MeasureStorage) -> Result<Self, MeasureRuntimeError> {
        Self::new_with_clock_and_evidence(
            storage,
            Box::new(UnavailableEvidenceStorage),
            None,
            Arc::new(SharedMeasureClock(Arc::new(SystemClock))),
        )
        .await
    }

    /// Load snapshots and bind restored evidence to this local collector.
    ///
    /// Pre: `evidence_storage` preserves a successful `put` across process restart. A volatile
    /// test backend is valid for bounded tests, but does not satisfy the crash-recovery claim.
    pub async fn new_with_evidence_storage(
        storage: MeasureStorage,
        evidence_storage: EvidenceStorage,
        collector: EvidenceCollectorIdentity,
    ) -> Result<Self, MeasureRuntimeError> {
        Self::new_with_evidence_storage_and_clock(
            storage,
            evidence_storage,
            collector,
            Arc::new(SystemClock),
        )
        .await
    }

    /// [`Self::new_with_evidence_storage`] reading epoch timestamps from `clock`.
    pub async fn new_with_evidence_storage_and_clock(
        storage: MeasureStorage,
        evidence_storage: EvidenceStorage,
        collector: EvidenceCollectorIdentity,
        clock: SharedClock,
    ) -> Result<Self, MeasureRuntimeError> {
        Self::new_with_clock_and_evidence(
            storage,
            evidence_storage,
            Some(collector),
            Arc::new(SharedMeasureClock(clock)),
        )
        .await
    }

    #[cfg(all(test, feature = "node"))]
    async fn new_with_clock(
        storage: MeasureStorage,
        clock: Arc<dyn MeasureClock>,
    ) -> Result<Self, MeasureRuntimeError> {
        Self::new_with_clock_and_evidence(
            storage,
            Box::new(rings_core::storage::MemStorage::new()),
            None,
            clock,
        )
        .await
    }

    /// Load persisted state with an explicit collector binding and default compaction threshold.
    async fn new_with_clock_and_evidence(
        storage: MeasureStorage,
        evidence_storage: EvidenceStorage,
        collector: Option<EvidenceCollectorIdentity>,
        clock: Arc<dyn MeasureClock>,
    ) -> Result<Self, MeasureRuntimeError> {
        Self::new_with_clock_evidence_and_threshold(
            storage,
            evidence_storage,
            collector,
            clock,
            EVIDENCE_DELTA_COMPACTION_THRESHOLD,
        )
        .await
    }

    /// Load persisted state and replay post-checkpoint evidence deltas with an explicit
    /// compaction threshold.
    async fn new_with_clock_evidence_and_threshold(
        storage: MeasureStorage,
        evidence_storage: EvidenceStorage,
        collector: Option<EvidenceCollectorIdentity>,
        clock: Arc<dyn MeasureClock>,
        evidence_compaction_threshold: usize,
    ) -> Result<Self, MeasureRuntimeError> {
        let spawner = Spawner::current()?;
        let storage = SharedMeasureStorage::from(storage);
        let evidence_storage = SharedEvidenceStorage::from(evidence_storage);
        let mut ledger = match storage.get(SNAPSHOT_KEY).await? {
            Some(snapshot) => MeasurementLedger::from_snapshot(snapshot)?,
            None => MeasurementLedger::new(),
        };
        let now = clock.now();
        let raw_evidence_entries = evidence_storage.get_all().await?;
        let restored = restore_evidence_from_entries(raw_evidence_entries, collector)?;
        if restored.load_report.rejected_records() > 0
            || restored.load_report.evicted_records() > 0
            || restored.load_report.rejected_replay_markers() > 0
        {
            tracing::warn!(
                rejected_records = restored.load_report.rejected_records(),
                evicted_records = restored.load_report.evicted_records(),
                rejected_replay_markers = restored.load_report.rejected_replay_markers(),
                "reconciled provisional evidence during startup"
            );
        }
        let reconciliation = ledger.reconcile_runtime(now, reliability_policy());
        if reconciliation.is_adjusted() {
            tracing::warn!(
                clock_adjusted_records = reconciliation.clock_adjusted_records(),
                reliability_reset_records = reconciliation.reliability_reset_records(),
                "reconciled measurement state during startup"
            );
        }
        let pruning = ledger.prune(now, CreditPolicy::amule());
        log_prune_failures(&pruning);
        let dirty = reconciliation.is_adjusted() || pruning.removed_count() > 0;
        let evidence_compaction_threshold = evidence_compaction_threshold.max(1);
        let evidence_dirty = restored.load_report.rejected_records() > 0
            || restored.load_report.evicted_records() > 0
            || restored.load_report.rejected_replay_markers() > 0
            || restored.has_stale_deltas
            || restored.pending_deltas.len() >= evidence_compaction_threshold;
        let next_prune_at = next_prune_time(&ledger, now);
        let (released_signal, released) = oneshot::channel();
        let state = Arc::new(MeasureState {
            storage,
            evidence_storage,
            evidence_compaction_threshold,
            runtime: Mutex::new(RuntimeLedger {
                ledger,
                evidence: restored.evidence,
                committed_evidence_override: None,
                next_evidence_delta_seq: restored.next_delta_seq,
                pending_evidence_deltas: restored.pending_deltas,
                dirty,
                persisting: false,
                mutated_while_persisting: false,
                evidence_dirty,
                evidence_persisting: false,
                evidence_mutated_while_persisting: false,
                last_clock: now,
                next_prune_at,
            }),
            ledger_turns: WriteTurnQueue::default(),
            evidence_turns: WriteTurnQueue::default(),
            pending_flushes: Mutex::new(Vec::new()),
            clock,
            spawner,
            tasks: TaskSet::new(),
            _released: released_signal,
        });
        let (mut persistence_wake, receiver) = mpsc::channel(PERSISTENCE_WAKE_CAPACITY);
        drop(state.tasks.spawn_with(
            &state.spawner,
            run_persistence_worker(state.clone(), receiver),
        ));
        if dirty || evidence_dirty {
            let _ = persistence_wake.try_send(());
        }
        Ok(Self {
            state,
            persistence_wake,
            released,
        })
    }

    /// Stop this measurement and wait until it has released its stores.
    ///
    /// The persistence worker sees its wake channel close, persists the pending snapshot as on
    /// a drop, and exits; a flush still running ends first.
    ///
    /// Post: no task of this measurement holds either store, so their directories can be opened
    /// again in this process (one open store per directory, #1039). A plain drop releases them
    /// too, but only once a later poll of the worker has written the final snapshot, so an open
    /// right after a drop can find the directory still held (#1083). A storage operation whose
    /// wait was abandoned (a timed-out [`Self::flush_with_timeout`]) holds its store until it
    /// ends.
    pub async fn close(self) {
        let Self {
            state,
            persistence_wake,
            released,
        } = self;
        drop(persistence_wake);
        drop(state);
        // The sender is never sent on: `Canceled` is the release.
        let _ = released.await;
    }

    /// Persist a snapshot containing every update visible when this method starts.
    ///
    /// On native targets, the Tokio runtime captured by [`Self::new`] must
    /// remain alive until the returned future completes.
    pub async fn flush(&self) -> Result<(), MeasureRuntimeError> {
        self.start_flush().await
    }

    /// Persist all applied updates unless the supplied deadline expires first.
    ///
    /// On native targets, the Tokio runtime captured by [`Self::new`] must
    /// remain alive until the returned future completes or reaches `timeout`.
    pub async fn flush_with_timeout(&self, timeout: Duration) -> Result<(), MeasureRuntimeError> {
        let flush = self.start_flush().fuse();
        let deadline = measurement_delay(timeout).fuse();
        futures::pin_mut!(flush, deadline);
        futures::select! {
            result = flush => result,
            deadline = deadline => match deadline {
                Ok(()) => Err(MeasureRuntimeError::FlushTimeout),
                Err(error) => Err(error),
            },
        }
    }

    /// Start a flush on the captured executor now, coalescing behind any flush pass not yet
    /// started; the flush outlives a caller that stops waiting for it (a timed-out
    /// [`Self::flush_with_timeout`]).
    fn start_flush(&self) -> impl std::future::Future<Output = Result<(), MeasureRuntimeError>> {
        let (tx, rx) = oneshot::channel();
        let should_spawn = {
            let mut pending = lock_or_recover(&self.state.pending_flushes);
            let should_spawn = pending.is_empty();
            pending.push(tx);
            should_spawn
        };
        if should_spawn {
            let pending_guard = PendingFlushGuard::new(Arc::clone(&self.state));
            drop(
                self.state
                    .tasks
                    .spawn_with(&self.state.spawner, async move {
                        run_coalesced_flush(pending_guard).await;
                    }),
            );
        }
        async move {
            match rx.await {
                Ok(result) => result,
                Err(_) => Err(MeasureRuntimeError::FlushTaskStopped),
            }
        }
    }

    fn wake_persistence(&self) {
        let mut sender = self.persistence_wake.clone();
        match sender.try_send(()) {
            Ok(()) => {}
            Err(error) if error.is_full() => {}
            Err(error) => tracing::error!(%error, "measurement persistence worker stopped"),
        }
    }
}

struct LedgerPersistGuard<'a> {
    state: &'a MeasureState,
    armed: bool,
}

impl Drop for LedgerPersistGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut runtime = lock_or_recover(&self.state.runtime);
            runtime.persisting = false;
            runtime.mutated_while_persisting = false;
        }
    }
}

struct EvidencePersistGuard<'a> {
    state: &'a MeasureState,
    armed: bool,
}

impl Drop for EvidencePersistGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut runtime = lock_or_recover(&self.state.runtime);
            runtime.evidence_persisting = false;
            runtime.evidence_mutated_while_persisting = false;
        }
    }
}

struct EvidenceAdmissionCancelGuard<'a> {
    wake: &'a mpsc::Sender<()>,
    armed: bool,
}

impl Drop for EvidenceAdmissionCancelGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut sender = self.wake.clone();
            let _ = sender.try_send(());
        }
    }
}

struct PendingFlushGuard {
    state: Arc<MeasureState>,
    armed: bool,
}

impl PendingFlushGuard {
    const fn new(state: Arc<MeasureState>) -> Self {
        Self { state, armed: true }
    }

    fn take_waiters(&mut self) -> Vec<oneshot::Sender<Result<(), MeasureRuntimeError>>> {
        let waiters = std::mem::take(&mut *lock_or_recover(&self.state.pending_flushes));
        self.armed = false;
        waiters
    }
}

impl Drop for PendingFlushGuard {
    fn drop(&mut self) {
        if self.armed {
            let waiters = std::mem::take(&mut *lock_or_recover(&self.state.pending_flushes));
            drop(waiters);
        }
    }
}

fn clone_core_error(error: &rings_core::error::Error) -> rings_core::error::Error {
    use rings_core::error::Error as CoreError;
    match error {
        CoreError::ServiceIOError(io) => {
            CoreError::ServiceIOError(std::io::Error::new(io.kind(), io.to_string()))
        }
        CoreError::StorageValueExceedsCapacity { required, capacity } => {
            CoreError::StorageValueExceedsCapacity {
                required: *required,
                capacity: *capacity,
            }
        }
        CoreError::StorageBudgetExhausted {
            used,
            required,
            capacity,
        } => CoreError::StorageBudgetExhausted {
            used: *used,
            required: *required,
            capacity: *capacity,
        },
        CoreError::StorageCountOverflow => CoreError::StorageCountOverflow,
        CoreError::StorageRecordUndecodable(record) => {
            CoreError::StorageRecordUndecodable(record.clone())
        }
        CoreError::StorageRecordMissing(record) => CoreError::StorageRecordMissing(record.clone()),
        CoreError::StorageDirectoryLocked(path) => CoreError::StorageDirectoryLocked(path.clone()),
        other => CoreError::ServiceIOError(std::io::Error::other(other.to_string())),
    }
}

fn clone_runtime_error(error: &MeasureRuntimeError) -> MeasureRuntimeError {
    match error {
        MeasureRuntimeError::Storage(core_err) => {
            MeasureRuntimeError::Storage(clone_core_error(core_err))
        }
        MeasureRuntimeError::Model(err) => MeasureRuntimeError::Model(*err),
        MeasureRuntimeError::Evidence(err) => MeasureRuntimeError::Evidence(*err),
        MeasureRuntimeError::FlushTimeout => MeasureRuntimeError::FlushTimeout,
        MeasureRuntimeError::FlushTaskStopped => MeasureRuntimeError::FlushTaskStopped,
        MeasureRuntimeError::Timer(err) => MeasureRuntimeError::Timer(err.clone()),
        MeasureRuntimeError::RuntimeUnavailable(err) => {
            MeasureRuntimeError::RuntimeUnavailable(*err)
        }
    }
}

async fn run_coalesced_flush(mut pending_guard: PendingFlushGuard) {
    let state = Arc::clone(&pending_guard.state);
    let ledger_turn = state.ledger_turns.acquire().await;
    let waiters = pending_guard.take_waiters();
    if waiters.is_empty() {
        return;
    }
    let result = flush_state_with_ledger_turn(&state, ledger_turn).await;
    match result {
        Ok(()) => {
            for waiter in waiters {
                let _ = waiter.send(Ok(()));
            }
        }
        Err(error) => {
            let mut iter = waiters.into_iter().peekable();
            while let Some(waiter) = iter.next() {
                if iter.peek().is_none() {
                    let _ = waiter.send(Err(error));
                    break;
                }
                let _ = waiter.send(Err(clone_runtime_error(&error)));
            }
        }
    }
}

async fn persist_ledger_with_turn(
    state: &MeasureState,
    only_if_dirty: bool,
) -> Result<(), MeasureRuntimeError> {
    let snapshot = {
        let mut runtime = lock_or_recover(&state.runtime);
        if only_if_dirty && !runtime.dirty {
            return Ok(());
        }
        prepare_ledger_snapshot(&mut runtime)
    };
    let mut guard = LedgerPersistGuard { state, armed: true };
    let result = state.storage.put(SNAPSHOT_KEY, &snapshot).await;
    guard.armed = false;
    finish_ledger_persist(&mut lock_or_recover(&state.runtime), result.is_ok());
    result.map_err(MeasureRuntimeError::from)
}

/// Remove compacted incremental evidence keys from `evidence_storage`, returning any sequence
/// numbers whose deletion failed so callers can keep cleanup dirty for retry.
async fn prune_compacted_delta_keys(
    evidence_storage: &SharedEvidenceStorage,
    keys_to_prune: &[(u64, String)],
) -> Vec<u64> {
    let mut failed_seqs = Vec::new();
    for (seq, key) in keys_to_prune {
        if let Err(error) = evidence_storage.remove(key).await {
            tracing::warn!(
                %error,
                key,
                sequence = seq,
                "failed to prune compacted provisional evidence delta"
            );
            failed_seqs.push(*seq);
        }
    }
    failed_seqs
}

/// Write a compacted provisional-evidence snapshot checkpoint and prune covered delta keys.
async fn persist_pending_evidence(
    state: &MeasureState,
    force_compact_deltas: bool,
) -> Result<(), MeasureRuntimeError> {
    let _turn = state.evidence_turns.acquire().await;
    let (entry, compacted_through_sequence, keys_to_prune) = {
        let mut runtime = lock_or_recover(&state.runtime);
        if !evidence_persist_needed(&runtime, force_compact_deltas) {
            return Ok(());
        }
        prepare_evidence_snapshot(&mut runtime)
    };
    let mut guard = EvidencePersistGuard { state, armed: true };
    let result = state
        .evidence_storage
        .put(EVIDENCE_SNAPSHOT_KEY, &entry)
        .await;
    if result.is_ok() {
        let failed_seqs = prune_compacted_delta_keys(&state.evidence_storage, &keys_to_prune).await;
        guard.armed = false;
        let mut runtime = lock_or_recover(&state.runtime);
        apply_compaction_cleanup(
            &mut runtime,
            compacted_through_sequence,
            &failed_seqs,
            state.evidence_compaction_threshold,
        );
        finish_evidence_persist(&mut runtime, true);
        if !failed_seqs.is_empty()
            || runtime.pending_evidence_deltas.len() >= state.evidence_compaction_threshold
        {
            runtime.evidence_dirty = true;
        }
        return Ok(());
    }
    guard.armed = false;
    finish_evidence_persist(&mut lock_or_recover(&state.runtime), false);
    result.map_err(MeasureRuntimeError::from)
}

/// Flush the measurement ledger under `ledger_turn` and compact any pending evidence state.
async fn flush_state_with_ledger_turn(
    state: &MeasureState,
    ledger_turn: WriteTurnGuard<'_>,
) -> Result<(), MeasureRuntimeError> {
    let ledger_result = persist_ledger_with_turn(state, false).await;
    drop(ledger_turn);
    ledger_result?;
    if evidence_persist_needed(&lock_or_recover(&state.runtime), true) {
        persist_pending_evidence(state, true).await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "node"))]
async fn flush_state(state: &MeasureState) -> Result<(), MeasureRuntimeError> {
    let ledger_turn = state.ledger_turns.acquire().await;
    flush_state_with_ledger_turn(state, ledger_turn).await
}

/// Wait on the shared runtime timer, reporting failure as a measurement runtime error.
async fn measurement_delay(duration: Duration) -> Result<(), MeasureRuntimeError> {
    Ok(sleep(duration).await?)
}

fn next_prune_time(ledger: &MeasurementLedger<Did>, now: UnixTime) -> UnixTime {
    ledger
        .next_retention_boundary(CreditPolicy::amule())
        .unwrap_or_else(|| {
            UnixTime::from_secs(now.as_secs().saturating_add(PRUNE_INTERVAL_SECONDS))
        })
}

fn reconcile_runtime_clock(
    runtime: &mut RuntimeLedger,
    now: UnixTime,
) -> Result<bool, MeasureError> {
    if now >= runtime.last_clock {
        runtime.last_clock = now;
        return Ok(false);
    }
    runtime.last_clock = now;
    let reconciliation = runtime.ledger.reconcile_runtime(now, reliability_policy());
    runtime.next_prune_at = next_prune_time(&runtime.ledger, now);
    if !reconciliation.is_adjusted() {
        return Ok(false);
    }
    mark_runtime_dirty(runtime);
    tracing::warn!(
        clock_adjusted_records = reconciliation.clock_adjusted_records(),
        reliability_reset_records = reconciliation.reliability_reset_records(),
        "reconciled measurement state after wall-clock regression"
    );
    Ok(true)
}

fn maintain_runtime(runtime: &mut RuntimeLedger, now: UnixTime) -> Result<bool, MeasureError> {
    let mut persistence_required = reconcile_runtime_clock(runtime, now)?;
    if now < runtime.next_prune_at {
        return Ok(persistence_required);
    }

    let pruning = runtime.ledger.prune(now, CreditPolicy::amule());
    log_prune_failures(&pruning);
    runtime.next_prune_at = next_prune_time(&runtime.ledger, now);
    if pruning.removed_count() > 0 {
        mark_runtime_dirty(runtime);
        persistence_required = true;
    }
    Ok(persistence_required)
}

fn mark_runtime_dirty(runtime: &mut RuntimeLedger) {
    runtime.dirty = true;
    if runtime.persisting {
        runtime.mutated_while_persisting = true;
    }
}

fn mark_evidence_dirty(runtime: &mut RuntimeLedger) {
    runtime.evidence_dirty = true;
    if runtime.evidence_persisting {
        runtime.evidence_mutated_while_persisting = true;
    }
}

/// Return whether evidence state requires writing a compacted snapshot checkpoint.
fn evidence_persist_needed(runtime: &RuntimeLedger, force_compact_deltas: bool) -> bool {
    runtime.evidence_dirty
        || runtime.committed_evidence_override.is_some()
        || (force_compact_deltas && !runtime.pending_evidence_deltas.is_empty())
}

/// Capture an immutable measurement ledger snapshot and mark persistence in flight.
fn prepare_ledger_snapshot(runtime: &mut RuntimeLedger) -> MeasurementSnapshot<Did> {
    runtime.persisting = true;
    runtime.mutated_while_persisting = false;
    runtime.ledger.snapshot()
}

/// Build a compacted provisional-evidence snapshot entry together with its sequence watermark
/// and currently pending delta keys eligible for pruning after commit.
fn prepare_evidence_snapshot(
    runtime: &mut RuntimeLedger,
) -> (EvidenceStorageEntry<Did>, u64, Vec<(u64, String)>) {
    runtime.evidence_persisting = true;
    runtime.evidence_mutated_while_persisting = false;
    let compacted_through_sequence = runtime.next_evidence_delta_seq.saturating_sub(1);
    let entry = EvidenceStorageEntry::Snapshot {
        compacted_through_sequence,
        snapshot: runtime.evidence.snapshot(),
    };
    let keys_to_prune = runtime.pending_evidence_deltas.clone();
    (entry, compacted_through_sequence, keys_to_prune)
}

/// Drop compacted delta entries through `compacted_through_sequence` (except `failed_seqs`) and
/// report whether follow-up compaction/cleanup remains necessary.
fn apply_compaction_cleanup(
    runtime: &mut RuntimeLedger,
    compacted_through_sequence: u64,
    failed_seqs: &[u64],
    compaction_threshold: usize,
) -> bool {
    runtime
        .pending_evidence_deltas
        .retain(|(seq, _)| *seq > compacted_through_sequence || failed_seqs.contains(seq));
    !failed_seqs.is_empty() || runtime.pending_evidence_deltas.len() >= compaction_threshold
}

/// Reconstructed in-memory evidence store and journal bookkeeping loaded during startup.
struct RestoredEvidenceState {
    /// Restored bounded provisional-evidence state after checkpoint and delta replay.
    evidence: ProvisionalEvidenceStore<Did>,
    /// Merged counts of rejected or evicted entries observed during restore.
    load_report: EvidenceLoadReport,
    /// Pending `(sequence, key)` delta/replay-floor entries present in storage.
    pending_deltas: Vec<(u64, String)>,
    /// Next monotonic sequence number strictly above both checkpoint and journal entries.
    next_delta_seq: u64,
    /// Whether orphaned or pre-checkpoint entries remain in storage requiring cleanup.
    has_stale_deltas: bool,
}

/// Reconstruct the bounded provisional-evidence store from the base snapshot checkpoint and
/// any strictly newer delta or replay-floor journal entries in deterministic sequence order.
fn restore_evidence_from_entries(
    raw_entries: Vec<(String, EvidenceStorageEntry<Did>)>,
    collector: Option<EvidenceCollectorIdentity>,
) -> Result<RestoredEvidenceState, EvidenceError> {
    let mut base_snapshot: Option<(u64, EvidenceSnapshot<Did>)> = None;
    let mut pending_deltas: Vec<(u64, String)> = Vec::new();
    let mut replay_entries: Vec<(u64, String, EvidenceStorageEntry<Did>)> = Vec::new();
    let mut has_stale_deltas = false;

    for (key, entry) in raw_entries {
        if key == EVIDENCE_SNAPSHOT_KEY {
            match entry {
                EvidenceStorageEntry::Snapshot {
                    compacted_through_sequence,
                    snapshot,
                } => {
                    base_snapshot = Some((compacted_through_sequence, snapshot));
                }
                _other => {
                    tracing::warn!(
                        key,
                        "ignoring unexpected non-snapshot provisional evidence entry under snapshot key"
                    );
                    has_stale_deltas = true;
                }
            }
            continue;
        }
        match &entry {
            EvidenceStorageEntry::Delta(delta) => {
                let seq = delta.sequence;
                pending_deltas.push((seq, key.clone()));
                replay_entries.push((seq, key, entry));
            }
            EvidenceStorageEntry::ReplayFloor { sequence, .. } => {
                let seq = *sequence;
                pending_deltas.push((seq, key.clone()));
                replay_entries.push((seq, key, entry));
            }
            EvidenceStorageEntry::Snapshot {
                compacted_through_sequence,
                ..
            } => {
                tracing::warn!(
                    key,
                    compacted_through_sequence,
                    "ignoring stray provisional evidence snapshot under non-snapshot key"
                );
                has_stale_deltas = true;
                pending_deltas.push((0, key));
            }
        }
    }

    let (base_seq, (mut evidence, mut load_report)) = match base_snapshot {
        Some((compacted_through_sequence, snapshot)) => (
            compacted_through_sequence,
            ProvisionalEvidenceStore::from_snapshot_with_validator(
                snapshot,
                EvidenceLimits::default(),
                |record| {
                    collector.is_some_and(|identity| valid_persisted_evidence(record, identity))
                },
                |marker| {
                    collector
                        .is_some_and(|identity| valid_persisted_replay_marker(marker, identity))
                },
            )?,
        ),
        None => (
            0,
            (
                ProvisionalEvidenceStore::new(EvidenceLimits::default()),
                EvidenceLoadReport::default(),
            ),
        ),
    };

    let mut max_seq = base_seq;
    replay_entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (seq, _key, entry) in replay_entries {
        max_seq = max_seq.max(seq);
        if seq <= base_seq {
            has_stale_deltas = true;
            continue;
        }
        match entry {
            EvidenceStorageEntry::Delta(delta) => {
                let report = evidence.replay_persisted_delta(delta, |record| {
                    collector.is_some_and(|identity| valid_persisted_evidence(record, identity))
                })?;
                load_report.merge(report);
            }
            EvidenceStorageEntry::ReplayFloor {
                schema_version,
                sequence: _,
                replay_floor,
            } => {
                if schema_version != EVIDENCE_SNAPSHOT_VERSION {
                    return Err(EvidenceError::UnsupportedSnapshotVersion {
                        found: schema_version,
                    });
                }
                evidence.restore_replay_floor(replay_floor);
            }
            EvidenceStorageEntry::Snapshot { .. } => {}
        }
    }

    pending_deltas.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let next_delta_seq = max_seq.saturating_add(1).max(1);

    Ok(RestoredEvidenceState {
        evidence,
        load_report,
        pending_deltas,
        next_delta_seq,
        has_stale_deltas,
    })
}

fn valid_persisted_evidence(
    record: &ProvisionalEvidenceRecord<Did>,
    collector: EvidenceCollectorIdentity,
) -> bool {
    let Ok(receipt) = rings_core::message::ProvisionalServiceReceipt::from_canonical_bytes(
        record.canonical_receipt(),
    ) else {
        return false;
    };
    let claim = &receipt.claim;
    let observed_at_ms = u128::from(record.observed_at().as_secs()) * 1_000;
    let observed_slot =
        rings_core::message::ProvisionalEpoch::from_unix_seconds(record.observed_at().as_secs())
            .slot;
    receipt
        .verify_live_at(collector.network_id, observed_at_ms)
        .is_ok()
        && claim.network_id == collector.network_id
        && claim.provider_account == collector.provider_account
        && claim.network_id == record.freshness().network_id()
        && claim.provider_account == *record.pair().provider()
        && claim.beneficiary_account == *record.pair().beneficiary()
        && claim.beneficiary_account == *record.freshness().beneficiary()
        && claim.epoch.slot == record.freshness().epoch_slot()
        && claim.nonce == record.freshness().nonce()
        && record.replay_floor() == observed_slot.saturating_sub(1)
        && receipt
            .digest()
            .is_ok_and(|digest| digest.into_bytes() == record.digest().into_bytes())
}

fn valid_persisted_replay_marker(
    marker: &EvidenceReplayMarker<Did>,
    collector: EvidenceCollectorIdentity,
) -> bool {
    marker.freshness().network_id() == collector.network_id
        && *marker.pair().provider() == collector.provider_account
        && marker.pair().beneficiary() == marker.freshness().beneficiary()
}

fn finish_ledger_persist(runtime: &mut RuntimeLedger, succeeded: bool) {
    runtime.persisting = false;
    if succeeded && !runtime.mutated_while_persisting {
        runtime.dirty = false;
    }
    runtime.mutated_while_persisting = false;
}

fn finish_evidence_persist(runtime: &mut RuntimeLedger, succeeded: bool) {
    runtime.evidence_persisting = false;
    if succeeded {
        runtime.committed_evidence_override = None;
    }
    if succeeded && !runtime.evidence_mutated_while_persisting {
        runtime.evidence_dirty = false;
    }
    runtime.evidence_mutated_while_persisting = false;
}

fn log_prune_failures(report: &rings_measure::PruneReport<Did>) {
    for failure in report.failures() {
        tracing::warn!(
            peer = %failure.peer(),
            error = %failure.error(),
            "retained peer measurement that could not be pruned"
        );
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn run_persistence_worker(state: Arc<MeasureState>, mut receiver: mpsc::Receiver<()>) {
    let mut retrying = false;
    loop {
        let should_attempt = if retrying {
            wait_for_retry_or_close(&mut receiver).await
        } else {
            match receiver.next().await {
                Some(()) => wait_for_debounce_or_close(&mut receiver).await,
                None => false,
            }
        };
        if !should_attempt {
            break;
        }
        retrying = match persist_pending_once(&state).await {
            Ok(()) => false,
            Err(error) => {
                tracing::error!(%error, "failed to persist measurement snapshot; retrying");
                true
            }
        };
    }
    persist_final_with_retries(&state).await;
}

async fn wait_for_debounce_or_close(receiver: &mut mpsc::Receiver<()>) -> bool {
    wait_for_debounce_or_close_with_delay(receiver, measurement_delay(PERSISTENCE_MIN_INTERVAL))
        .await
}

async fn wait_for_debounce_or_close_with_delay(
    receiver: &mut mpsc::Receiver<()>,
    delay: impl std::future::Future<Output = Result<(), MeasureRuntimeError>>,
) -> bool {
    let delay = delay.fuse();
    futures::pin_mut!(delay);
    loop {
        let signal = receiver.next().fuse();
        futures::pin_mut!(signal);
        futures::select! {
            result = delay => {
                log_persistence_delay_error(result);
                return true;
            }
            signal = signal => match signal {
                Some(()) => {}
                None => return false,
            }
        }
    }
}

async fn wait_for_retry_or_close(receiver: &mut mpsc::Receiver<()>) -> bool {
    wait_for_retry_or_close_with_delay(receiver, measurement_delay(PERSISTENCE_MIN_INTERVAL)).await
}

async fn wait_for_retry_or_close_with_delay(
    receiver: &mut mpsc::Receiver<()>,
    delay: impl std::future::Future<Output = Result<(), MeasureRuntimeError>>,
) -> bool {
    let delay = delay.fuse();
    futures::pin_mut!(delay);
    let mut wake_observed = false;
    loop {
        let signal = receiver.next().fuse();
        futures::pin_mut!(signal);
        futures::select! {
            result = delay => {
                match result {
                    Ok(()) => return true,
                    Err(error) => {
                        tracing::error!(%error, "failed to schedule measurement persistence retry");
                        if wake_observed {
                            return true;
                        }
                        // A broken browser timer cannot provide bounded autonomous retry. Wait for
                        // a later semantic mutation instead of spinning the JS microtask queue.
                        return receiver.next().await.is_some();
                    }
                }
            }
            signal = signal => match signal {
                Some(()) => wake_observed = true,
                None => return false,
            }
        }
    }
}

async fn persist_final_with_retries(state: &MeasureState) {
    // Shutdown retries are deliberately back-to-back: this path must remain
    // bounded and can recover immediate backend races, but must not add another
    // timer dependency while the owning runtime is stopping.
    for attempt in 1..=PERSISTENCE_SHUTDOWN_ATTEMPTS {
        match persist_pending_once(state).await {
            Ok(()) => return,
            Err(error) => {
                tracing::error!(
                    %error,
                    attempt,
                    max_attempts = PERSISTENCE_SHUTDOWN_ATTEMPTS,
                    "failed to persist final measurement snapshot"
                );
            }
        }
    }
}

fn log_persistence_delay_error(result: Result<(), MeasureRuntimeError>) {
    if let Err(error) = result {
        tracing::error!(%error, "failed to debounce measurement persistence");
    }
}

/// Flush dirty ledger or dirty/compaction-pending evidence state once for the background worker.
async fn persist_pending_once(state: &MeasureState) -> Result<(), MeasureRuntimeError> {
    let (ledger_dirty, evidence_dirty) = {
        let runtime = lock_or_recover(&state.runtime);
        (runtime.dirty, evidence_persist_needed(&runtime, false))
    };
    if ledger_dirty {
        let _turn = state.ledger_turns.acquire().await;
        persist_ledger_with_turn(state, true).await?;
    }
    if evidence_dirty {
        persist_pending_evidence(state, false).await?;
    }
    Ok(())
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
impl Measure for PeriodicMeasure {
    async fn record(
        &self,
        did: Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) -> Result<ApplyOutcome, MeasureError> {
        self.record_batch(did, authentication, MeasurementBatch::single(event))
            .await
    }

    async fn record_batch(
        &self,
        did: Did,
        authentication: Authentication,
        batch: MeasurementBatch,
    ) -> Result<ApplyOutcome, MeasureError> {
        let (outcome, persistence_required) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let persistence_required = maintain_runtime(&mut runtime, now)?;
            let outcome = runtime
                .ledger
                .apply_batch(did, authentication, batch, now, reliability_policy())
                .map(|report| {
                    if let Some(evicted_peer) = report.evicted_peer() {
                        tracing::warn!(
                            ?evicted_peer,
                            replacement_peer = ?did,
                            "measurement ledger evicted its stalest authenticated peer"
                        );
                    }
                    report.outcome()
                });
            let applied = matches!(outcome, Ok(ApplyOutcome::Applied));
            if applied {
                mark_runtime_dirty(&mut runtime);
            }
            (outcome, persistence_required || applied)
        };
        if persistence_required {
            self.wake_persistence();
        }
        outcome
    }

    async fn peer_measurement(&self, did: Did) -> Result<Option<PeerMeasurement>, MeasureError> {
        let (projected, reconciled) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let reconciled = maintain_runtime(&mut runtime, now)?;
            let projected =
                runtime
                    .ledger
                    .measurement(&did, now, CreditPolicy::amule(), reliability_policy());
            (projected, reconciled)
        };
        if reconciled {
            self.wake_persistence();
        }
        Ok(projected?.map(PeerMeasurement::from_projected))
    }

    async fn peer_measurements(&self) -> Result<Vec<PeerMeasurement>, MeasureError> {
        let (projection, reconciled) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let reconciled = maintain_runtime(&mut runtime, now)?;
            let projection =
                runtime
                    .ledger
                    .measurements(now, CreditPolicy::amule(), reliability_policy());
            (projection, reconciled)
        };
        if reconciled {
            self.wake_persistence();
        }
        let (measurements, failures) = projection.into_parts();
        for failure in failures {
            tracing::warn!(
                peer = %failure.peer(),
                error = %failure.error(),
                "omitted invalid peer from measurement projection"
            );
        }
        Ok(measurements
            .into_iter()
            .map(PeerMeasurement::from_projected)
            .collect())
    }

    async fn peer_measurements_page(
        &self,
        after: Option<Did>,
        limit: NonZeroUsize,
    ) -> Result<PeerMeasurementPage, MeasureError> {
        let (page, reconciled) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let reconciled = maintain_runtime(&mut runtime, now)?;
            let page = runtime.ledger.measurements_page(
                after.as_ref(),
                limit,
                now,
                CreditPolicy::amule(),
                reliability_policy(),
            );
            (page, reconciled)
        };
        if reconciled {
            self.wake_persistence();
        }
        let (measurements, failures, next_cursor) = page.into_parts();
        for failure in failures {
            tracing::warn!(
                peer = %failure.peer(),
                error = %failure.error(),
                "omitted invalid peer from bounded measurement projection"
            );
        }
        Ok(PeerMeasurementPage {
            measurements: measurements
                .into_iter()
                .map(PeerMeasurement::from_projected)
                .collect(),
            next_cursor,
        })
    }

    async fn admit_provisional_evidence(
        &self,
        record: ProvisionalEvidenceRecord<Did>,
    ) -> Result<EvidenceAdmissionReport<Did>, EvidenceError> {
        enum AdmissionWritePlan {
            Snapshot {
                entry: EvidenceStorageEntry<Did>,
                compacted_through_sequence: u64,
                keys_to_prune: Vec<(u64, String)>,
            },
            Incremental {
                key: String,
                entry: EvidenceStorageEntry<Did>,
            },
            NoWrite,
        }

        // The evidence-storage put below is the successful operation's linearization point.
        // Law (Lock order): evidence_turns ≺ runtime. While the durable write is in flight,
        // `committed_evidence_override` preserves the pre-admission view for read-only queries so
        // they never block behind storage I/O and never observe an uncommitted admission.
        let _evidence_turn = self.state.evidence_turns.acquire().await;
        let (
            rollback_before,
            previous_override,
            previous_evidence_dirty,
            previous_pending_len,
            result,
            plan,
        ) = {
            let mut runtime = lock_or_recover(&self.state.runtime);
            let rollback_before = Arc::new(runtime.evidence.clone());
            let previous_override = runtime.committed_evidence_override.clone();
            let previous_evidence_dirty = runtime.evidence_dirty;
            let previous_replay_floor = runtime.evidence.replay_floor();
            let previous_pending_len = runtime.pending_evidence_deltas.len();
            let result = runtime.evidence.admit(record.clone());

            let stage_incremental = |runtime: &mut RuntimeLedger| {
                if runtime.committed_evidence_override.is_none() {
                    runtime.committed_evidence_override = Some(Arc::clone(&rollback_before));
                }
                mark_evidence_dirty(runtime);
                let sequence = runtime.next_evidence_delta_seq;
                runtime.next_evidence_delta_seq = sequence.saturating_add(1);
                let key = evidence_delta_key(sequence);
                runtime
                    .pending_evidence_deltas
                    .push((sequence, key.clone()));
                (sequence, key)
            };

            let plan = if previous_override.is_some() || previous_evidence_dirty {
                // Reconcile prior cancelled/dirty state with a compacted Snapshot checkpoint.
                // Checked before the NoWrite branch so a Duplicate retry reconciles cleanly.
                if runtime.committed_evidence_override.is_none() {
                    runtime.committed_evidence_override = Some(Arc::clone(&rollback_before));
                }
                mark_evidence_dirty(&mut runtime);
                let compacted_through_sequence = runtime.next_evidence_delta_seq.saturating_sub(1);
                let keys_to_prune = runtime.pending_evidence_deltas.clone();
                AdmissionWritePlan::Snapshot {
                    entry: EvidenceStorageEntry::Snapshot {
                        compacted_through_sequence,
                        snapshot: runtime.evidence.snapshot(),
                    },
                    compacted_through_sequence,
                    keys_to_prune,
                }
            } else if matches!(
                &result,
                Ok(report) if report.admission() == EvidenceAdmission::Admitted
            ) {
                let (sequence, key) = stage_incremental(&mut runtime);
                AdmissionWritePlan::Incremental {
                    key,
                    entry: EvidenceStorageEntry::Delta(EvidenceAdmissionDelta {
                        schema_version: EVIDENCE_SNAPSHOT_VERSION,
                        sequence,
                        record,
                    }),
                }
            } else if runtime.evidence.replay_floor() > previous_replay_floor {
                let (sequence, key) = stage_incremental(&mut runtime);
                AdmissionWritePlan::Incremental {
                    key,
                    entry: EvidenceStorageEntry::ReplayFloor {
                        schema_version: EVIDENCE_SNAPSHOT_VERSION,
                        sequence,
                        replay_floor: runtime.evidence.replay_floor(),
                    },
                }
            } else {
                AdmissionWritePlan::NoWrite
            };

            (
                rollback_before,
                previous_override,
                previous_evidence_dirty,
                previous_pending_len,
                result,
                plan,
            )
        };

        let mut cancel_guard = EvidenceAdmissionCancelGuard {
            wake: &self.persistence_wake,
            armed: true,
        };
        /// Restore pre-admission in-memory evidence state and pending delta list after a failed write.
        #[allow(clippy::too_many_arguments)]
        fn rollback_admission(
            state: &MeasureState,
            error: rings_core::error::Error,
            guard: &mut EvidenceAdmissionCancelGuard<'_>,
            previous_override: Option<Arc<ProvisionalEvidenceStore<Did>>>,
            rollback_before: Arc<ProvisionalEvidenceStore<Did>>,
            previous_evidence_dirty: bool,
            previous_pending_len: usize,
        ) -> EvidenceError {
            guard.armed = false;
            tracing::error!(%error, "failed to commit provisional evidence admission");
            let mut runtime = lock_or_recover(&state.runtime);
            runtime.committed_evidence_override = previous_override;
            runtime.evidence =
                Arc::try_unwrap(rollback_before).unwrap_or_else(|arc| (*arc).clone());
            runtime.evidence_dirty = previous_evidence_dirty;
            runtime
                .pending_evidence_deltas
                .truncate(previous_pending_len);
            EvidenceError::PersistenceUnavailable
        }

        match plan {
            AdmissionWritePlan::Snapshot {
                entry,
                compacted_through_sequence,
                keys_to_prune,
            } => {
                if let Err(error) = self
                    .state
                    .evidence_storage
                    .put(EVIDENCE_SNAPSHOT_KEY, &entry)
                    .await
                {
                    return Err(rollback_admission(
                        &self.state,
                        error,
                        &mut cancel_guard,
                        previous_override,
                        rollback_before,
                        previous_evidence_dirty,
                        previous_pending_len,
                    ));
                }
                let failed_seqs =
                    prune_compacted_delta_keys(&self.state.evidence_storage, &keys_to_prune).await;
                cancel_guard.armed = false;
                let should_compact = {
                    let mut runtime = lock_or_recover(&self.state.runtime);
                    runtime.committed_evidence_override = None;
                    let dirty = apply_compaction_cleanup(
                        &mut runtime,
                        compacted_through_sequence,
                        &failed_seqs,
                        self.state.evidence_compaction_threshold,
                    );
                    runtime.evidence_dirty = dirty;
                    dirty
                };
                if should_compact {
                    self.wake_persistence();
                }
            }
            AdmissionWritePlan::Incremental { key, entry } => {
                if let Err(error) = self.state.evidence_storage.put(&key, &entry).await {
                    return Err(rollback_admission(
                        &self.state,
                        error,
                        &mut cancel_guard,
                        previous_override,
                        rollback_before,
                        previous_evidence_dirty,
                        previous_pending_len,
                    ));
                }
                cancel_guard.armed = false;
                let should_compact = {
                    let mut runtime = lock_or_recover(&self.state.runtime);
                    runtime.committed_evidence_override = None;
                    let should_compact = runtime.pending_evidence_deltas.len()
                        >= self.state.evidence_compaction_threshold;
                    runtime.evidence_dirty = previous_evidence_dirty || should_compact;
                    should_compact
                };
                if should_compact {
                    self.wake_persistence();
                }
            }
            AdmissionWritePlan::NoWrite => {
                cancel_guard.armed = false;
            }
        }
        result
    }

    async fn provisional_evidence_page(
        &self,
        after: Option<EvidenceDigest>,
        limit: NonZeroUsize,
    ) -> Result<EvidencePage<Did>, EvidenceError> {
        // Leaf read (`runtime` only, `Law (Lock order): evidence_turns ≺ runtime`).
        Ok(lock_or_recover(&self.state.runtime)
            .committed_evidence()
            .page(after, limit))
    }

    async fn provisional_evidence_counters(&self) -> EvidenceCounters {
        // Leaf read (`runtime` only, `Law (Lock order): evidence_turns ≺ runtime`).
        lock_or_recover(&self.state.runtime)
            .committed_evidence()
            .counters()
    }
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
impl measure::BehaviourJudgement for PeriodicMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        match self.peer_measurement(did).await {
            Ok(Some(measurement)) => measurement.quality,
            Ok(None) => PeerQuality::Unknown,
            Err(error) => {
                tracing::error!(peer = %did, %error, "failed to project peer reliability");
                PeerQuality::Unknown
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod authentication_tests;
#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod evidence_tests;
#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod tests;
#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod worker_tests;
