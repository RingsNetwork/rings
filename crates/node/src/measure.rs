//! Runtime adapter for the pure `rings-measure` state relation.

use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::lock::Mutex as AsyncMutex;
use futures::FutureExt;
use futures::StreamExt;
use rings_core::dht::Did;
use rings_core::measure;
use rings_core::measure::Measure;
use rings_core::measure::MeasureCounter;
use rings_core::measure::PeerMeasurement;
use rings_core::measure::PeerMeasurementPage;
use rings_core::measure::PeerQuality;
use rings_core::measure::PeerQualityThresholds;
use rings_core::storage::KvStorageInterface;
use rings_measure::ApplyOutcome;
use rings_measure::Authentication;
use rings_measure::CreditPolicy;
use rings_measure::EvidenceAdmissionReport;
use rings_measure::EvidenceCounters;
use rings_measure::EvidenceDigest;
use rings_measure::EvidenceError;
use rings_measure::EvidenceLimits;
use rings_measure::EvidencePage;
use rings_measure::EvidenceReplayMarker;
use rings_measure::EvidenceSnapshot;
use rings_measure::MeasureError;
use rings_measure::MeasurementBatch;
use rings_measure::MeasurementEvent;
use rings_measure::MeasurementLedger;
use rings_measure::MeasurementSnapshot;
use rings_measure::ProvisionalEvidenceRecord;
use rings_measure::ProvisionalEvidenceStore;
use rings_measure::ReliabilityPolicy;
use rings_measure::UnixTime;

// Legacy `PeriodicMeasure/counters/...` values intentionally remain unread:
// a bare count proves neither byte-credit direction nor a live epoch timestamp.
const SNAPSHOT_KEY: &str = "MeasurementLedger/v1";
const EVIDENCE_SNAPSHOT_KEY: &str = "ProvisionalEvidence/v1";
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
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub type MeasureStorage = Box<dyn KvStorageInterface<MeasurementSnapshot<Did>>>;

/// Storage used for one versioned complete measurement snapshot.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub type MeasureStorage = Box<dyn KvStorageInterface<MeasurementSnapshot<Did>> + Sync + Send>;

/// Storage used for the separate provisional-receipt evidence snapshot.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub type EvidenceStorage = Box<dyn KvStorageInterface<EvidenceSnapshot<Did>>>;

/// Storage used for the separate provisional-receipt evidence snapshot.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub type EvidenceStorage = Box<dyn KvStorageInterface<EvidenceSnapshot<Did>> + Sync + Send>;

/// Evidence backend used when a provider did not configure durable receipt storage.
///
/// Reads expose an empty initial state so measurement-only runtimes can start, but every write
/// fails. Consequently the admission adapter rolls back and never returns `Admitted` under a
/// process-local store that cannot refine the crash-recovery model.
pub(crate) struct UnavailableEvidenceStorage;

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl KvStorageInterface<EvidenceSnapshot<Did>> for UnavailableEvidenceStorage {
    async fn get(&self, _key: &str) -> rings_core::error::Result<Option<EvidenceSnapshot<Did>>> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &str,
        value: &EvidenceSnapshot<Did>,
    ) -> rings_core::error::Result<()> {
        if value.records.is_empty()
            && value.replay_markers.is_empty()
            && value.replay_floor == 0
            && value.counters == EvidenceCounters::default()
        {
            return Ok(());
        }
        Err(EvidenceError::StorageUnavailable.into())
    }

    async fn get_all(&self) -> rings_core::error::Result<Vec<(String, EvidenceSnapshot<Did>)>> {
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

#[cfg(all(feature = "browser", target_family = "wasm"))]
type SharedMeasureStorage = Arc<dyn KvStorageInterface<MeasurementSnapshot<Did>>>;
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
type SharedMeasureStorage = Arc<dyn KvStorageInterface<MeasurementSnapshot<Did>> + Sync + Send>;

#[cfg(all(feature = "browser", target_family = "wasm"))]
type SharedEvidenceStorage = Arc<dyn KvStorageInterface<EvidenceSnapshot<Did>>>;
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
type SharedEvidenceStorage = Arc<dyn KvStorageInterface<EvidenceSnapshot<Did>> + Sync + Send>;

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
    Timer(String),
    /// Native construction was attempted without a live Tokio runtime.
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    #[error("measurement persistence requires a live Tokio runtime: {0}")]
    RuntimeUnavailable(String),
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
}

struct MeasureState {
    storage: SharedMeasureStorage,
    evidence_storage: SharedEvidenceStorage,
    runtime: Mutex<RuntimeLedger>,
    persistence_lock: AsyncMutex<()>,
    clock: Arc<dyn MeasureClock>,
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    runtime_handle: tokio::runtime::Handle,
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
    dirty: bool,
    persisting: bool,
    mutated_while_persisting: bool,
    last_clock: UnixTime,
    next_prune_at: UnixTime,
}

// Boundary: the adapter supplies wall-clock seconds to the pure state relation.
trait MeasureClock: Send + Sync {
    fn now(&self) -> UnixTime;
}

struct SystemMeasureClock;

impl MeasureClock for SystemMeasureClock {
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    fn now(&self) -> UnixTime {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        UnixTime::from_secs(seconds)
    }

    #[cfg(all(feature = "browser", target_family = "wasm"))]
    fn now(&self) -> UnixTime {
        let milliseconds = js_sys::Date::now();
        if !milliseconds.is_finite() || milliseconds <= 0.0 {
            return UnixTime::EPOCH;
        }
        UnixTime::from_secs((milliseconds / 1_000.0) as u64)
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
            Arc::new(SystemMeasureClock),
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
        Self::new_with_clock_and_evidence(
            storage,
            evidence_storage,
            Some(collector),
            Arc::new(SystemMeasureClock),
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

    async fn new_with_clock_and_evidence(
        storage: MeasureStorage,
        evidence_storage: EvidenceStorage,
        collector: Option<EvidenceCollectorIdentity>,
        clock: Arc<dyn MeasureClock>,
    ) -> Result<Self, MeasureRuntimeError> {
        #[cfg(not(all(feature = "browser", target_family = "wasm")))]
        let runtime_handle = tokio::runtime::Handle::try_current()
            .map_err(|error| MeasureRuntimeError::RuntimeUnavailable(error.to_string()))?;
        let storage = SharedMeasureStorage::from(storage);
        let evidence_storage = SharedEvidenceStorage::from(evidence_storage);
        let mut ledger = match storage.get(SNAPSHOT_KEY).await? {
            Some(snapshot) => MeasurementLedger::from_snapshot(snapshot)?,
            None => MeasurementLedger::new(),
        };
        let now = clock.now();
        let (evidence, evidence_load) = match evidence_storage.get(EVIDENCE_SNAPSHOT_KEY).await? {
            Some(snapshot) => ProvisionalEvidenceStore::from_snapshot_with_validator(
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
            None => (
                ProvisionalEvidenceStore::new(EvidenceLimits::default()),
                rings_measure::EvidenceLoadReport::default(),
            ),
        };
        if evidence_load.rejected_records() > 0
            || evidence_load.evicted_records() > 0
            || evidence_load.rejected_replay_markers() > 0
        {
            tracing::warn!(
                rejected_records = evidence_load.rejected_records(),
                evicted_records = evidence_load.evicted_records(),
                rejected_replay_markers = evidence_load.rejected_replay_markers(),
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
        let dirty = reconciliation.is_adjusted()
            || pruning.removed_count() > 0
            || evidence_load.rejected_records() > 0
            || evidence_load.evicted_records() > 0
            || evidence_load.rejected_replay_markers() > 0;
        let next_prune_at = next_prune_time(&ledger, now);
        let state = Arc::new(MeasureState {
            storage,
            evidence_storage,
            runtime: Mutex::new(RuntimeLedger {
                ledger,
                evidence,
                dirty,
                persisting: false,
                mutated_while_persisting: false,
                last_clock: now,
                next_prune_at,
            }),
            persistence_lock: AsyncMutex::new(()),
            clock,
            #[cfg(not(all(feature = "browser", target_family = "wasm")))]
            runtime_handle,
        });
        let (mut persistence_wake, receiver) = mpsc::channel(PERSISTENCE_WAKE_CAPACITY);
        spawn_persistence_worker(state.clone(), receiver);
        if dirty {
            let _ = persistence_wake.try_send(());
        }
        Ok(Self {
            state,
            persistence_wake,
        })
    }

    /// Persist a snapshot containing every update visible when this method starts.
    ///
    /// On native targets, the Tokio runtime captured by [`Self::new`] must
    /// remain alive until the returned future completes.
    pub async fn flush(&self) -> Result<(), MeasureRuntimeError> {
        let (sender, flush) = futures::channel::oneshot::channel();
        spawn_bounded_flush(self.state.clone(), sender);
        match flush.await {
            Ok(result) => result,
            Err(_) => Err(MeasureRuntimeError::FlushTaskStopped),
        }
    }

    /// Persist all applied updates unless the supplied deadline expires first.
    ///
    /// On native targets, the Tokio runtime captured by [`Self::new`] must
    /// remain alive until the returned future completes or reaches `timeout`.
    pub async fn flush_with_timeout(&self, timeout: Duration) -> Result<(), MeasureRuntimeError> {
        let (sender, flush) = futures::channel::oneshot::channel();
        spawn_bounded_flush(self.state.clone(), sender);
        let flush = flush.fuse();
        let deadline = measurement_delay(timeout).fuse();
        futures::pin_mut!(flush, deadline);
        futures::select! {
            result = flush => match result {
                Ok(result) => result,
                Err(_) => Err(MeasureRuntimeError::FlushTaskStopped),
            },
            deadline = deadline => match deadline {
                Ok(()) => Err(MeasureRuntimeError::FlushTimeout),
                Err(error) => Err(error),
            },
        }
    }

    fn count(&self, did: Did, counter: MeasureCounter) -> u64 {
        let (measurement, reconciled) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let reconciled = match maintain_runtime(&mut runtime, now) {
                Ok(reconciled) => reconciled,
                Err(error) => {
                    tracing::error!(peer = %did, %error, "failed to maintain measurement state");
                    return 0;
                }
            };
            let measurement =
                runtime
                    .ledger
                    .measurement(&did, now, CreditPolicy::amule(), reliability_policy());
            (measurement, reconciled)
        };
        if reconciled {
            self.wake_persistence();
        }
        let measurement = match measurement {
            Ok(Some(measurement)) => measurement,
            Ok(None) => return 0,
            Err(error) => {
                tracing::warn!(peer = %did, %error, "failed to project measurement counter");
                return 0;
            }
        };
        let evidence = measurement.reliability;
        match counter {
            MeasureCounter::Sent => evidence.sent,
            MeasureCounter::FailedToSend => evidence.failed_to_send,
            MeasureCounter::Received => evidence.received,
            MeasureCounter::FailedToReceive => evidence.failed_to_receive,
            MeasureCounter::Connect => evidence.connected,
            MeasureCounter::Disconnected => evidence.disconnected,
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

async fn flush_state(state: &MeasureState) -> Result<(), MeasureRuntimeError> {
    let _guard = state.persistence_lock.lock().await;
    let (snapshot, evidence_snapshot) = {
        let mut runtime = lock_or_recover(&state.runtime);
        prepare_snapshots(&mut runtime)
    };
    let result = match state.storage.put(SNAPSHOT_KEY, &snapshot).await {
        Ok(()) => {
            state
                .evidence_storage
                .put(EVIDENCE_SNAPSHOT_KEY, &evidence_snapshot)
                .await
        }
        Err(error) => Err(error),
    };
    finish_persist(&mut lock_or_recover(&state.runtime), result.is_ok());
    result.map_err(MeasureRuntimeError::from)
}

type FlushSender = futures::channel::oneshot::Sender<Result<(), MeasureRuntimeError>>;

#[cfg(not(all(feature = "browser", target_family = "wasm")))]
fn spawn_bounded_flush(state: Arc<MeasureState>, sender: FlushSender) {
    let runtime_handle = state.runtime_handle.clone();
    runtime_handle.spawn(async move {
        let _ = sender.send(flush_state(&state).await);
    });
}

#[cfg(not(all(feature = "browser", target_family = "wasm")))]
async fn measurement_delay(duration: Duration) -> Result<(), MeasureRuntimeError> {
    futures_timer::Delay::new(duration).await;
    Ok(())
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
async fn measurement_delay(duration: Duration) -> Result<(), MeasureRuntimeError> {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    rings_core::utils::js_utils::window_sleep(millis)
        .await
        .map_err(|error| MeasureRuntimeError::Timer(format!("{error:?}")))?;
    Ok(())
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn spawn_bounded_flush(state: Arc<MeasureState>, sender: FlushSender) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = sender.send(flush_state(&state).await);
    });
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

fn prepare_snapshots(
    runtime: &mut RuntimeLedger,
) -> (MeasurementSnapshot<Did>, EvidenceSnapshot<Did>) {
    runtime.persisting = true;
    runtime.mutated_while_persisting = false;
    (runtime.ledger.snapshot(), runtime.evidence.snapshot())
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

fn finish_persist(runtime: &mut RuntimeLedger, succeeded: bool) {
    runtime.persisting = false;
    if succeeded && !runtime.mutated_while_persisting {
        runtime.dirty = false;
    }
    runtime.mutated_while_persisting = false;
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

#[cfg(not(all(feature = "browser", target_family = "wasm")))]
fn spawn_persistence_worker(state: Arc<MeasureState>, receiver: mpsc::Receiver<()>) {
    let runtime_handle = state.runtime_handle.clone();
    runtime_handle.spawn(run_persistence_worker(state, receiver));
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn spawn_persistence_worker(state: Arc<MeasureState>, receiver: mpsc::Receiver<()>) {
    wasm_bindgen_futures::spawn_local(run_persistence_worker(state, receiver));
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

async fn persist_pending_once(state: &MeasureState) -> Result<(), MeasureRuntimeError> {
    let _guard = state.persistence_lock.lock().await;
    let (snapshot, evidence_snapshot) = {
        let mut runtime = lock_or_recover(&state.runtime);
        if !runtime.dirty {
            return Ok(());
        }
        prepare_snapshots(&mut runtime)
    };
    let result = match state.storage.put(SNAPSHOT_KEY, &snapshot).await {
        Ok(()) => {
            state
                .evidence_storage
                .put(EVIDENCE_SNAPSHOT_KEY, &evidence_snapshot)
                .await
        }
        Err(error) => Err(error),
    };
    finish_persist(&mut lock_or_recover(&state.runtime), result.is_ok());
    result.map_err(MeasureRuntimeError::from)
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
impl Measure for PeriodicMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        if let Err(error) = self
            .record(did, Authentication::Authenticated, counter.into_event())
            .await
        {
            tracing::error!(peer = %did, %error, "failed to apply compatibility measurement");
        }
    }

    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        self.count(did, counter)
    }

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
        // The evidence-storage put below is the successful operation's linearization point.
        // Queries and other admissions take this same lock, so an explicit write failure is
        // rolled back before it becomes observable. Cancellation may conservatively leave the
        // dirty in-memory marker for the worker to persist, which preserves at-most-once admission.
        let _persistence_guard = self.state.persistence_lock.lock().await;
        let (before, result, snapshot) = {
            let mut runtime = lock_or_recover(&self.state.runtime);
            let before = runtime.evidence.clone();
            let result = runtime.evidence.admit(record);
            mark_runtime_dirty(&mut runtime);
            let snapshot = runtime.evidence.snapshot();
            (before, result, snapshot)
        };
        self.wake_persistence();
        if let Err(error) = self
            .state
            .evidence_storage
            .put(EVIDENCE_SNAPSHOT_KEY, &snapshot)
            .await
        {
            tracing::error!(%error, "failed to commit provisional evidence admission");
            let mut runtime = lock_or_recover(&self.state.runtime);
            runtime.evidence = before;
            return Err(EvidenceError::PersistenceUnavailable);
        }
        result
    }

    async fn provisional_evidence_page(
        &self,
        after: Option<EvidenceDigest>,
        limit: NonZeroUsize,
    ) -> Result<EvidencePage<Did>, EvidenceError> {
        let _persistence_guard = self.state.persistence_lock.lock().await;
        Ok(lock_or_recover(&self.state.runtime)
            .evidence
            .page(after, limit))
    }

    async fn provisional_evidence_counters(&self) -> EvidenceCounters {
        let _persistence_guard = self.state.persistence_lock.lock().await;
        lock_or_recover(&self.state.runtime).evidence.counters()
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
