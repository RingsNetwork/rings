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
        let reconciliation = ledger.reconcile_clock(now, reliability_policy());
        if reconciliation.is_adjusted() {
            tracing::warn!(
                adjusted_records = reconciliation.adjusted_records(),
                "reconciled future measurement timestamps during startup"
            );
        }
        let pruning = ledger.prune(now, CreditPolicy::amule());
        log_prune_failures(&pruning);
        let dirty = reconciliation.is_adjusted() || pruning.removed_count() > 0;
        let state = Arc::new(MeasureState {
            storage,
            runtime: Mutex::new(RuntimeLedger {
                ledger,
                dirty,
                last_clock: now,
                next_prune_at: next_prune_time(now),
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
        flush_state(&self.state).await
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
            let reconciled = reconcile_runtime_clock(&mut runtime, now);
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
        runtime.dirty = false;
        runtime.ledger.snapshot()
    };
    if let Err(error) = state.storage.put(SNAPSHOT_KEY, &snapshot).await {
        lock_or_recover(&state.runtime).dirty = true;
        return Err(error.into());
    }
    Ok(())
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

fn next_prune_time(now: UnixTime) -> UnixTime {
    UnixTime::from_secs(now.as_secs().saturating_add(PRUNE_INTERVAL_SECONDS))
}

fn reconcile_runtime_clock(runtime: &mut RuntimeLedger, now: UnixTime) -> bool {
    if now >= runtime.last_clock {
        runtime.last_clock = now;
        return false;
    }
    runtime.last_clock = now;
    runtime.next_prune_at = next_prune_time(now);
    let reconciliation = runtime.ledger.reconcile_clock(now, reliability_policy());
    if !reconciliation.is_adjusted() {
        return false;
    }
    runtime.dirty = true;
    tracing::warn!(
        adjusted_records = reconciliation.adjusted_records(),
        "reconciled future measurement timestamps after wall-clock regression"
    );
    true
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
    while receiver.next().await.is_some() {
        if let Err(error) = measurement_delay(PERSISTENCE_MIN_INTERVAL).await {
            tracing::error!(%error, "failed to debounce measurement persistence");
        }
        persist_pending_once(&state).await;
    }
    persist_pending_once(&state).await;
}

async fn persist_pending_once(state: &MeasureState) {
    let _guard = state.persistence_lock.lock().await;
    let snapshot = {
        let mut runtime = lock_or_recover(&state.runtime);
        if !runtime.dirty {
            return;
        }
        runtime.dirty = false;
        runtime.ledger.snapshot()
    };
    if let Err(error) = state.storage.put(SNAPSHOT_KEY, &snapshot).await {
        lock_or_recover(&state.runtime).dirty = true;
        tracing::error!(%error, "failed to persist measurement snapshot");
    }
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
impl Measure for PeriodicMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        if let Err(error) = self.record(did, counter.into_event()).await {
            tracing::error!(peer = %did, %error, "failed to apply compatibility measurement");
        }
    }

    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        self.count(did, counter)
    }

    async fn record(
        &self,
        did: Did,
        event: MeasurementEvent,
    ) -> Result<ApplyOutcome, MeasureError> {
        let outcome = {
            let (mut runtime, now) = self.state.runtime_at_now();
            reconcile_runtime_clock(&mut runtime, now);
            let outcome = runtime.ledger.apply(
                did,
                Authentication::Authenticated,
                event,
                now,
                reliability_policy(),
            )?;
            if now >= runtime.next_prune_at {
                let pruning = runtime.ledger.prune(now, CreditPolicy::amule());
                log_prune_failures(&pruning);
                runtime.next_prune_at = next_prune_time(now);
            }
            runtime.dirty = true;
            outcome
        };
        self.wake_persistence();
        Ok(outcome)
    }

    async fn peer_measurement(&self, did: Did) -> Result<Option<PeerMeasurement>, MeasureError> {
        let (projected, reconciled) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let reconciled = reconcile_runtime_clock(&mut runtime, now);
            let projected = runtime.ledger.measurement(
                &did,
                now,
                CreditPolicy::amule(),
                reliability_policy(),
            )?;
            (projected, reconciled)
        };
        if reconciled {
            self.wake_persistence();
        }
        Ok(projected.map(PeerMeasurement::from_projected))
    }

    async fn peer_measurements(&self) -> Result<Vec<PeerMeasurement>, MeasureError> {
        let (projection, reconciled) = {
            let (mut runtime, now) = self.state.runtime_at_now();
            let reconciled = reconcile_runtime_clock(&mut runtime, now);
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
            let reconciled = reconcile_runtime_clock(&mut runtime, now);
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
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use rings_core::error::Error as CoreError;
    use rings_core::measure::BehaviourJudgement;
    use rings_core::storage::sled::SledStorage;
    use rings_core::storage::KvStorageInterface;
    use rings_core::storage::MemStorage;
    use tokio::sync::Notify;
    use tokio::sync::Semaphore;

    use super::*;

    struct ManualMeasureClock {
        now: AtomicU64,
    }

    impl ManualMeasureClock {
        const fn new(now: u64) -> Self {
            Self {
                now: AtomicU64::new(now),
            }
        }

        fn advance(&self, seconds: u64) {
            self.now.fetch_add(seconds, Ordering::SeqCst);
        }

        fn set(&self, seconds: u64) {
            self.now.store(seconds, Ordering::SeqCst);
        }
    }

    impl MeasureClock for ManualMeasureClock {
        fn now(&self) -> UnixTime {
            UnixTime::from_secs(self.now.load(Ordering::SeqCst))
        }
    }

    #[derive(Clone, Default)]
    struct GatedStorage {
        state: Arc<GatedStorageState>,
    }

    struct GatedStorageState {
        value: Mutex<Option<MeasurementSnapshot<Did>>>,
        writes: AtomicU64,
        write_permits: Semaphore,
        release_first_write: Notify,
    }

    impl Default for GatedStorageState {
        fn default() -> Self {
            Self {
                value: Mutex::new(None),
                writes: AtomicU64::new(0),
                write_permits: Semaphore::new(0),
                release_first_write: Notify::new(),
            }
        }
    }

    impl GatedStorage {
        async fn wait_for_writes(&self, expected: u64) {
            while self.state.writes.load(Ordering::SeqCst) < expected {
                let permit = self
                    .state
                    .write_permits
                    .acquire()
                    .await
                    .unwrap_or_else(|error| panic!("write semaphore must remain open: {error}"));
                permit.forget();
            }
        }

        fn release_first_write(&self) {
            self.state.release_first_write.notify_one();
        }
    }

    #[async_trait]
    impl KvStorageInterface<MeasurementSnapshot<Did>> for GatedStorage {
        async fn get(
            &self,
            _key: &str,
        ) -> rings_core::error::Result<Option<MeasurementSnapshot<Did>>> {
            Ok(lock_or_recover(&self.state.value).clone())
        }

        async fn put(
            &self,
            _key: &str,
            value: &MeasurementSnapshot<Did>,
        ) -> rings_core::error::Result<()> {
            let previous = self.state.writes.fetch_add(1, Ordering::SeqCst);
            self.state.write_permits.add_permits(1);
            if previous == 0 {
                self.state.release_first_write.notified().await;
            }
            *lock_or_recover(&self.state.value) = Some(value.clone());
            Ok(())
        }

        async fn get_all(
            &self,
        ) -> rings_core::error::Result<Vec<(String, MeasurementSnapshot<Did>)>> {
            Ok(Vec::new())
        }

        async fn remove(&self, _key: &str) -> rings_core::error::Result<()> {
            *lock_or_recover(&self.state.value) = None;
            Ok(())
        }

        async fn clear(&self) -> rings_core::error::Result<()> {
            *lock_or_recover(&self.state.value) = None;
            Ok(())
        }

        async fn count(&self) -> rings_core::error::Result<u32> {
            Ok(u32::from(lock_or_recover(&self.state.value).is_some()))
        }
    }

    struct FailingStorage;

    #[async_trait]
    impl KvStorageInterface<MeasurementSnapshot<Did>> for FailingStorage {
        async fn get(
            &self,
            _key: &str,
        ) -> rings_core::error::Result<Option<MeasurementSnapshot<Did>>> {
            Ok(None)
        }

        async fn put(
            &self,
            _key: &str,
            _value: &MeasurementSnapshot<Did>,
        ) -> rings_core::error::Result<()> {
            Err(CoreError::InvalidTransport)
        }

        async fn get_all(
            &self,
        ) -> rings_core::error::Result<Vec<(String, MeasurementSnapshot<Did>)>> {
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

    fn did() -> Did {
        Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0")
            .unwrap_or_else(|error| panic!("test DID must parse: {error}"))
    }

    async fn memory_measure(clock: Arc<ManualMeasureClock>) -> PeriodicMeasure {
        PeriodicMeasure::new_with_clock(Box::new(MemStorage::new()), clock)
            .await
            .unwrap_or_else(|error| panic!("memory measurement must initialize: {error}"))
    }

    #[tokio::test]
    async fn compatibility_counters_follow_the_live_epoch() {
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = memory_measure(clock.clone()).await;
        measure.incr(did(), MeasureCounter::Sent).await;
        measure.incr(did(), MeasureCounter::Sent).await;
        assert_eq!(measure.get_count(did(), MeasureCounter::Sent).await, 2);
        clock.advance(DURATION);
        assert_eq!(measure.get_count(did(), MeasureCounter::Sent).await, 0);
    }

    #[tokio::test]
    async fn useful_bytes_produce_amule_credit_and_one_reliability_event() {
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = memory_measure(clock).await;
        assert_eq!(
            measure
                .record(did(), MeasurementEvent::Received {
                    useful_bytes: 2_000_000,
                },)
                .await,
            Ok(ApplyOutcome::Applied)
        );
        let projected = measure
            .peer_measurement(did())
            .await
            .unwrap_or_else(|error| panic!("projection must succeed: {error}"))
            .unwrap_or_else(|| panic!("measurement must exist"));
        assert_eq!(projected.evidence.received, 1);
        assert_eq!(
            projected
                .credit
                .map(|credit| credit.bytes_received_from_peer()),
            Some(2_000_000)
        );
        assert!(projected.credit_score.as_f64() > 1.0);
    }

    #[tokio::test]
    async fn persistence_restores_complete_credit_and_epoch_state() {
        let path = "tmp/measure_snapshot_test_db";
        let storage: MeasureStorage = Box::new(
            SledStorage::new_with_cap_and_path(4096, path)
                .await
                .unwrap_or_else(|error| panic!("test storage must open: {error}")),
        );
        storage
            .clear()
            .await
            .unwrap_or_else(|error| panic!("test storage must clear: {error}"));
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = PeriodicMeasure::new_with_clock(storage, clock.clone())
            .await
            .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
        measure
            .record(did(), MeasurementEvent::Sent { useful_bytes: 17 })
            .await
            .unwrap_or_else(|error| panic!("measurement must apply: {error}"));
        measure
            .flush()
            .await
            .unwrap_or_else(|error| panic!("measurement must flush: {error}"));
        drop(measure);

        let restored = PeriodicMeasure::new_with_clock(
            Box::new(
                SledStorage::new_with_cap_and_path(4096, path)
                    .await
                    .unwrap_or_else(|error| panic!("test storage must reopen: {error}")),
            ),
            clock,
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must restore: {error}"));
        let projected = restored
            .peer_measurement(did())
            .await
            .unwrap_or_else(|error| panic!("projection must succeed: {error}"))
            .unwrap_or_else(|| panic!("restored measurement must exist"));
        assert_eq!(projected.evidence.sent, 1);
        assert_eq!(
            projected.credit.map(|credit| credit.bytes_sent_to_peer()),
            Some(17)
        );
    }

    #[tokio::test]
    async fn startup_prunes_records_at_the_retention_boundary() {
        let storage = MemStorage::new();
        let mut ledger = MeasurementLedger::new();
        ledger
            .apply(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(10),
                reliability_policy(),
            )
            .unwrap_or_else(|error| panic!("fixture event must apply: {error}"));
        storage
            .put(SNAPSHOT_KEY, &ledger.snapshot())
            .await
            .unwrap_or_else(|error| panic!("fixture snapshot must persist: {error}"));

        let expiry = 10 + CreditPolicy::amule().retention_seconds();
        let measure = PeriodicMeasure::new_with_clock(
            Box::new(storage),
            Arc::new(ManualMeasureClock::new(expiry)),
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

        assert!(measure
            .peer_measurement(did())
            .await
            .unwrap_or_else(|error| panic!("projection must succeed: {error}"))
            .is_none());
    }

    #[tokio::test]
    async fn startup_reconciles_future_snapshot_timestamps() {
        let storage = MemStorage::new();
        let mut ledger = MeasurementLedger::new();
        ledger
            .apply(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(100),
                reliability_policy(),
            )
            .unwrap_or_else(|error| panic!("fixture event must apply: {error}"));
        storage
            .put(SNAPSHOT_KEY, &ledger.snapshot())
            .await
            .unwrap_or_else(|error| panic!("fixture snapshot must persist: {error}"));

        let measure = PeriodicMeasure::new_with_clock(
            Box::new(storage),
            Arc::new(ManualMeasureClock::new(20)),
        )
        .await
        .unwrap_or_else(|error| panic!("clock regression must not abort startup: {error}"));
        let projected = measure
            .peer_measurement(did())
            .await
            .unwrap_or_else(|error| panic!("projection must succeed: {error}"))
            .unwrap_or_else(|| panic!("reconciled record must remain visible"));

        assert_eq!(
            projected.credit.map(|credit| credit.last_seen()),
            Some(UnixTime::from_secs(20))
        );
        assert_eq!(projected.evidence.connected, 1);
    }

    #[tokio::test]
    async fn live_query_reconciles_a_wall_clock_regression() {
        let clock = Arc::new(ManualMeasureClock::new(100));
        let measure = memory_measure(clock.clone()).await;
        measure
            .record(did(), MeasurementEvent::Received { useful_bytes: 64 })
            .await
            .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));
        clock.set(20);

        let projected = measure
            .peer_measurement(did())
            .await
            .unwrap_or_else(|error| panic!("clock regression must reconcile: {error}"))
            .unwrap_or_else(|| panic!("reconciled record must remain visible"));

        assert_eq!(
            projected.credit.map(|credit| credit.last_seen()),
            Some(UnixTime::from_secs(20))
        );
        assert_eq!(projected.evidence.received, 1);
    }

    #[tokio::test]
    async fn bulk_query_includes_retained_peer_without_connection_enumeration() {
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = memory_measure(clock).await;
        measure.incr(did(), MeasureCounter::Connect).await;
        let all = measure
            .peer_measurements()
            .await
            .unwrap_or_else(|error| panic!("bulk projection must succeed: {error}"));
        assert_eq!(all.len(), 1);
        assert_eq!(all.first().map(|measurement| measurement.did), Some(did()));
    }

    #[tokio::test]
    async fn bounded_query_uses_an_exclusive_did_cursor() {
        let measure = memory_measure(Arc::new(ManualMeasureClock::new(10))).await;
        for did in [Did::from(3_u32), Did::from(1_u32), Did::from(2_u32)] {
            measure
                .record(did, MeasurementEvent::Connected)
                .await
                .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));
        }
        let limit = NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN);

        let first = measure
            .peer_measurements_page(None, limit)
            .await
            .unwrap_or_else(|error| panic!("first page must project: {error}"));
        assert_eq!(
            first
                .measurements
                .iter()
                .map(|measurement| measurement.did)
                .collect::<Vec<_>>(),
            vec![Did::from(1_u32), Did::from(2_u32)]
        );
        assert_eq!(first.next_cursor, Some(Did::from(2_u32)));

        let second = measure
            .peer_measurements_page(first.next_cursor, limit)
            .await
            .unwrap_or_else(|error| panic!("second page must project: {error}"));
        assert_eq!(
            second
                .measurements
                .first()
                .map(|measurement| measurement.did),
            Some(Did::from(3_u32))
        );
        assert_eq!(second.measurements.len(), 1);
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn repeated_disconnections_degrade_peer_quality() {
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = memory_measure(clock).await;
        assert_eq!(measure.quality(did()).await, PeerQuality::Unknown);
        measure.incr(did(), MeasureCounter::Connect).await;
        assert_eq!(measure.quality(did()).await, PeerQuality::Healthy);
        for _ in 0..crate::consts::CONNECT_FAILED_LIMIT {
            measure.incr(did(), MeasureCounter::Disconnected).await;
        }
        assert_eq!(measure.quality(did()).await, PeerQuality::Degraded);
    }

    #[tokio::test]
    async fn storage_latency_does_not_block_memory_first_updates() {
        let storage = GatedStorage::default();
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = PeriodicMeasure::new_with_clock(Box::new(storage.clone()), clock)
            .await
            .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

        tokio::time::timeout(
            Duration::from_millis(50),
            measure.record(did(), MeasurementEvent::Sent { useful_bytes: 23 }),
        )
        .await
        .unwrap_or_else(|_| panic!("storage latency must not block record"))
        .unwrap_or_else(|error| panic!("measurement must apply: {error}"));
        tokio::time::timeout(Duration::from_secs(1), storage.wait_for_writes(1))
            .await
            .unwrap_or_else(|_| panic!("persistence worker must attempt a write"));
        let projected = measure
            .peer_measurement(did())
            .await
            .unwrap_or_else(|error| panic!("projection must succeed: {error}"))
            .unwrap_or_else(|| panic!("memory-first measurement must exist"));
        assert_eq!(
            projected.credit.map(|credit| credit.bytes_sent_to_peer()),
            Some(23)
        );
        assert!(matches!(
            measure.flush_with_timeout(Duration::from_millis(10)).await,
            Err(MeasureRuntimeError::FlushTimeout)
        ));

        storage.release_first_write();
        measure
            .flush_with_timeout(Duration::from_secs(1))
            .await
            .unwrap_or_else(|error| panic!("released storage must flush: {error}"));
        assert!(storage.state.writes.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn persistence_enforces_a_minimum_interval_between_full_snapshots() {
        let storage = GatedStorage::default();
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = PeriodicMeasure::new_with_clock(Box::new(storage.clone()), clock)
            .await
            .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
        measure
            .record(did(), MeasurementEvent::Sent { useful_bytes: 1 })
            .await
            .unwrap_or_else(|error| panic!("first measurement must apply: {error}"));
        tokio::time::timeout(Duration::from_secs(1), storage.wait_for_writes(1))
            .await
            .unwrap_or_else(|_| panic!("first snapshot write must start"));

        for _ in 0..32 {
            measure
                .record(did(), MeasurementEvent::Sent { useful_bytes: 1 })
                .await
                .unwrap_or_else(|error| panic!("burst measurement must apply: {error}"));
        }
        storage.release_first_write();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), storage.wait_for_writes(2))
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), storage.wait_for_writes(2))
            .await
            .unwrap_or_else(|_| panic!("dirty burst must persist after the minimum interval"));
        assert_eq!(storage.state.writes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn storage_failure_is_observable_without_rolling_back_memory() {
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = PeriodicMeasure::new_with_clock(Box::new(FailingStorage), clock)
            .await
            .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
        assert_eq!(
            measure
                .record(did(), MeasurementEvent::Received { useful_bytes: 29 })
                .await,
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(measure.get_count(did(), MeasureCounter::Received).await, 1);
        assert!(matches!(
            measure.flush_with_timeout(Duration::from_secs(1)).await,
            Err(MeasureRuntimeError::Storage(CoreError::InvalidTransport))
        ));
        assert_eq!(measure.get_count(did(), MeasureCounter::Received).await, 1);
    }
}
