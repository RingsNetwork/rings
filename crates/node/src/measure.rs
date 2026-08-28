//! Runtime adapter for the pure `rings-measure` state relation.

use std::num::NonZeroU64;
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
use rings_core::measure::ApplyOutcome;
use rings_core::measure::Measure;
use rings_core::measure::MeasureCounter;
use rings_core::measure::MeasureError;
use rings_core::measure::MeasurementEvent;
use rings_core::measure::PeerMeasurement;
use rings_core::measure::PeerQuality;
use rings_core::measure::PeerQualityThresholds;
use rings_core::measure::UnixTime;
use rings_core::storage::KvStorageInterface;
use rings_measure::Authentication;
use rings_measure::CreditPolicy;
use rings_measure::MeasurementLedger;
use rings_measure::MeasurementSnapshot;
use rings_measure::ReliabilityPolicy;

#[cfg(test)]
const DURATION: u64 = 1;
#[cfg(not(test))]
const DURATION: u64 = 60 * 60;
// Legacy `PeriodicMeasure/counters/...` values intentionally remain unread:
// a bare count proves neither byte-credit direction nor a live epoch timestamp.
const SNAPSHOT_KEY: &str = "MeasurementLedger/v1";
const PERSISTENCE_WAKE_CAPACITY: usize = 1;
const PRUNE_INTERVAL_SECONDS: u64 = 60 * 60;
const RELIABILITY_WINDOW: NonZeroU64 = match NonZeroU64::new(DURATION) {
    Some(window) => window,
    None => NonZeroU64::MIN,
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

struct RuntimeLedger {
    ledger: MeasurementLedger<Did>,
    dirty: bool,
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
        let pruned = ledger.prune(now, CreditPolicy::amule())?;
        let state = Arc::new(MeasureState {
            storage,
            runtime: Mutex::new(RuntimeLedger {
                ledger,
                dirty: pruned > 0,
                next_prune_at: next_prune_time(now),
            }),
            persistence_lock: AsyncMutex::new(()),
            clock,
        });
        let (mut persistence_wake, receiver) = mpsc::channel(PERSISTENCE_WAKE_CAPACITY);
        spawn_persistence_worker(state.clone(), receiver);
        if pruned > 0 {
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
        let deadline = futures_timer::Delay::new(timeout).fuse();
        futures::pin_mut!(flush, deadline);
        futures::select! {
            result = flush => match result {
                Ok(result) => result,
                Err(_) => Err(MeasureRuntimeError::FlushTaskStopped),
            },
            () = deadline => Err(MeasureRuntimeError::FlushTimeout),
        }
    }

    fn count(&self, did: Did, counter: MeasureCounter) -> u64 {
        let now = self.state.clock.now();
        let runtime = lock_or_recover(&self.state.runtime);
        let measurement =
            runtime
                .ledger
                .measurement(&did, now, CreditPolicy::amule(), reliability_policy());
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

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn spawn_bounded_flush(state: Arc<MeasureState>, sender: FlushSender) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = sender.send(flush_state(&state).await);
    });
}

fn next_prune_time(now: UnixTime) -> UnixTime {
    UnixTime::from_secs(now.as_secs().saturating_add(PRUNE_INTERVAL_SECONDS))
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
        persist_pending(&state).await;
    }
    persist_pending(&state).await;
}

async fn persist_pending(state: &MeasureState) {
    loop {
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
            return;
        }
    }
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
impl Measure for PeriodicMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        let event = match counter {
            MeasureCounter::Sent => MeasurementEvent::Sent { useful_bytes: 0 },
            MeasureCounter::FailedToSend => MeasurementEvent::FailedToSend,
            MeasureCounter::Received => MeasurementEvent::Received { useful_bytes: 0 },
            MeasureCounter::FailedToReceive => MeasurementEvent::FailedToReceive,
            MeasureCounter::Connect => MeasurementEvent::Connected,
            MeasureCounter::Disconnected => MeasurementEvent::Disconnected,
        };
        if let Err(error) = self.record(did, event).await {
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
        let now = self.state.clock.now();
        let outcome = {
            let mut runtime = lock_or_recover(&self.state.runtime);
            let outcome = runtime.ledger.apply(
                did,
                Authentication::Authenticated,
                event,
                now,
                reliability_policy(),
            )?;
            if now >= runtime.next_prune_at {
                match runtime.ledger.prune(now, CreditPolicy::amule()) {
                    Ok(_) => runtime.next_prune_at = next_prune_time(now),
                    Err(error) => {
                        tracing::error!(%error, "failed to prune measurement ledger");
                    }
                }
            }
            runtime.dirty = true;
            outcome
        };
        let mut sender = self.persistence_wake.clone();
        match sender.try_send(()) {
            Ok(()) => {}
            Err(error) if error.is_full() => {}
            Err(error) => tracing::error!(%error, "measurement persistence worker stopped"),
        }
        Ok(outcome)
    }

    async fn peer_measurement(&self, did: Did) -> Result<Option<PeerMeasurement>, MeasureError> {
        let projected = lock_or_recover(&self.state.runtime).ledger.measurement(
            &did,
            self.state.clock.now(),
            CreditPolicy::amule(),
            reliability_policy(),
        )?;
        Ok(projected.map(PeerMeasurement::from_projected))
    }

    async fn peer_measurements(&self) -> Result<Vec<PeerMeasurement>, MeasureError> {
        lock_or_recover(&self.state.runtime)
            .ledger
            .measurements(
                self.state.clock.now(),
                CreditPolicy::amule(),
                reliability_policy(),
            )
            .map(|measurements| {
                measurements
                    .into_iter()
                    .map(PeerMeasurement::from_projected)
                    .collect()
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

    #[derive(Default)]
    struct GatedStorageState {
        value: Mutex<Option<MeasurementSnapshot<Did>>>,
        writes: AtomicU64,
        write_started: Notify,
        release_first_write: Notify,
    }

    impl GatedStorage {
        async fn wait_for_first_write(&self) {
            if self.state.writes.load(Ordering::SeqCst) == 0 {
                self.state.write_started.notified().await;
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
            if self.state.writes.fetch_add(1, Ordering::SeqCst) == 0 {
                self.state.write_started.notify_one();
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
        tokio::time::timeout(Duration::from_secs(1), storage.wait_for_first_write())
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
