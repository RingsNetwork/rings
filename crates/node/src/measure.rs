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
use rings_measure::MeasureError;
use rings_measure::MeasurementBatch;
use rings_measure::MeasurementEvent;
use rings_measure::MeasurementLedger;
use rings_measure::MeasurementSnapshot;
use rings_measure::ReliabilityPolicy;
use rings_measure::UnixTime;

#[cfg(test)]
const DURATION: u64 = 1;
#[cfg(not(test))]
const DURATION: u64 = 60 * 60;
// Legacy `PeriodicMeasure/counters/...` values intentionally remain unread:
// a bare count proves neither byte-credit direction nor a live epoch timestamp.
const SNAPSHOT_KEY: &str = "MeasurementLedger/v1";
const PERSISTENCE_WAKE_CAPACITY: usize = 1;
const PERSISTENCE_SHUTDOWN_ATTEMPTS: usize = 3;
const PRUNE_INTERVAL_SECONDS: u64 = 60 * 60;
#[cfg(test)]
const PERSISTENCE_MIN_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const PERSISTENCE_MIN_INTERVAL: Duration = Duration::from_secs(60);
const RELIABILITY_WINDOW: NonZeroU64 = match NonZeroU64::new(DURATION) {
    Some(window) => window,
    None => panic!("measurement reliability window must be non-zero"),
};

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

#[cfg(all(feature = "browser", target_family = "wasm"))]
type SharedMeasureStorage = Arc<dyn KvStorageInterface<MeasurementSnapshot<Did>>>;
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
type SharedMeasureStorage = Arc<dyn KvStorageInterface<MeasurementSnapshot<Did>> + Sync + Send>;

/// Failure while loading or explicitly flushing the runtime measurement adapter.
#[derive(Debug, thiserror::Error)]
pub enum MeasureRuntimeError {
    /// The configured key-value backend failed.
    #[error("measurement storage failed: {0}")]
    Storage(#[from] rings_core::error::Error),
    /// Persisted or live state violated the pure measurement model.
    #[error("measurement model failed: {0}")]
    Model(#[from] MeasureError),
    /// A bounded explicit flush did not complete before its deadline.
    #[error("measurement persistence flush timed out")]
    FlushTimeout,
    /// The runtime stopped the owned flush task before it returned a result.
    #[error("measurement persistence flush task stopped")]
    FlushTaskStopped,
    /// The browser could not schedule a measurement timer.
    #[error("measurement timer failed: {0}")]
    Timer(String),
}

/// Pure-ledger runtime adapter with coalesced asynchronous snapshot persistence.
///
/// Network callbacks update only in-memory state and replace the pending full
/// snapshot. A runtime task serializes storage writes. The algorithm, time
/// projection, pruning, and snapshot schema remain in `rings-measure`.
pub struct PeriodicMeasure {
    state: Arc<MeasureState>,
    persistence_wake: mpsc::Sender<()>,
}

struct MeasureState {
    storage: SharedMeasureStorage,
    runtime: Mutex<RuntimeLedger>,
    persistence_lock: AsyncMutex<()>,
    clock: Arc<dyn MeasureClock>,
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
    pub async fn new(storage: MeasureStorage) -> Result<Self, MeasureRuntimeError> {
        Self::new_with_clock(storage, Arc::new(SystemMeasureClock)).await
    }

    async fn new_with_clock(
        storage: MeasureStorage,
        clock: Arc<dyn MeasureClock>,
    ) -> Result<Self, MeasureRuntimeError> {
        let storage = SharedMeasureStorage::from(storage);
        let mut ledger = match storage.get(SNAPSHOT_KEY).await? {
            Some(snapshot) => MeasurementLedger::from_snapshot(snapshot)?,
            None => MeasurementLedger::new(),
        };
        let now = clock.now();
        let reconciliation = ledger.reconcile_clock(now, reliability_policy())?;
        if reconciliation.is_adjusted() {
            tracing::warn!(
                adjusted_records = reconciliation.adjusted_records(),
                "reconciled future measurement timestamps during startup"
            );
        }
        let pruning = ledger.prune(now, CreditPolicy::amule());
        log_prune_failures(&pruning);
        let dirty = reconciliation.is_adjusted() || pruning.removed_count() > 0;
        let next_prune_at = next_prune_time(&ledger, now);
        let state = Arc::new(MeasureState {
            storage,
            runtime: Mutex::new(RuntimeLedger {
                ledger,
                dirty,
                persisting: false,
                mutated_while_persisting: false,
                last_clock: now,
                next_prune_at,
            }),
            persistence_lock: AsyncMutex::new(()),
            clock,
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
    pub async fn flush(&self) -> Result<(), MeasureRuntimeError> {
        let (sender, flush) = futures::channel::oneshot::channel();
        spawn_bounded_flush(self.state.clone(), sender);
        match flush.await {
            Ok(result) => result,
            Err(_) => Err(MeasureRuntimeError::FlushTaskStopped),
        }
    }

    /// Persist all applied updates unless the supplied deadline expires first.
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
        let Ok(Some(measurement)) = measurement else {
            return 0;
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
    let snapshot = {
        let mut runtime = lock_or_recover(&state.runtime);
        prepare_snapshot(&mut runtime)
    };
    let result = state.storage.put(SNAPSHOT_KEY, &snapshot).await;
    finish_persist(&mut lock_or_recover(&state.runtime), result.is_ok());
    result.map_err(MeasureRuntimeError::from)
}

type FlushSender = futures::channel::oneshot::Sender<Result<(), MeasureRuntimeError>>;

#[cfg(not(all(feature = "browser", target_family = "wasm")))]
fn spawn_bounded_flush(state: Arc<MeasureState>, sender: FlushSender) {
    tokio::spawn(async move {
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
    let reconciliation = runtime.ledger.reconcile_clock(now, reliability_policy())?;
    runtime.next_prune_at = next_prune_time(&runtime.ledger, now);
    if !reconciliation.is_adjusted() {
        return Ok(false);
    }
    mark_runtime_dirty(runtime);
    tracing::warn!(
        adjusted_records = reconciliation.adjusted_records(),
        "reconciled future measurement timestamps after wall-clock regression"
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

fn prepare_snapshot(runtime: &mut RuntimeLedger) -> MeasurementSnapshot<Did> {
    runtime.persisting = true;
    runtime.mutated_while_persisting = false;
    runtime.ledger.snapshot()
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
    tokio::spawn(run_persistence_worker(state, receiver));
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
    let snapshot = {
        let mut runtime = lock_or_recover(&state.runtime);
        if !runtime.dirty {
            return Ok(());
        }
        prepare_snapshot(&mut runtime)
    };
    let result = state.storage.put(SNAPSHOT_KEY, &snapshot).await;
    finish_persist(&mut lock_or_recover(&state.runtime), result.is_ok());
    result.map_err(MeasureRuntimeError::from)
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
impl Measure for PeriodicMeasure {
    async fn incr(&self, did: Did, authentication: Authentication, counter: MeasureCounter) {
        if let Err(error) = self.record(did, authentication, counter.into_event()).await {
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
            let outcome =
                runtime
                    .ledger
                    .apply_batch(did, authentication, batch, now, reliability_policy());
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

    async fn good(&self, did: Did) -> bool {
        !matches!(self.quality(did).await, PeerQuality::Degraded)
    }
}

#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod authentication_tests;
#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod tests;
#[cfg(test)]
#[cfg(feature = "node")]
#[allow(clippy::panic)]
mod worker_tests;
