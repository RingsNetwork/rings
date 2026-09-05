use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use rings_core::error::Error as CoreError;
use rings_core::measure::BehaviourJudgement;
use rings_core::storage::file::FileStorage;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::MemStorage;
use tokio::sync::Notify;
use tokio::sync::Semaphore;

use super::*;

#[allow(
    clippy::unwrap_used,
    reason = "test helpers receive explicit non-zero literals"
)]
fn nonzero_usize(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

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
    async fn get(&self, _key: &str) -> rings_core::error::Result<Option<MeasurementSnapshot<Did>>> {
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

    async fn get_all(&self) -> rings_core::error::Result<Vec<(String, MeasurementSnapshot<Did>)>> {
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
    async fn get(&self, _key: &str) -> rings_core::error::Result<Option<MeasurementSnapshot<Did>>> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &str,
        _value: &MeasurementSnapshot<Did>,
    ) -> rings_core::error::Result<()> {
        Err(CoreError::InvalidTransport)
    }

    async fn get_all(&self) -> rings_core::error::Result<Vec<(String, MeasurementSnapshot<Did>)>> {
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

#[derive(Clone)]
struct TransientStorage {
    state: Arc<TransientStorageState>,
}

struct TransientStorageState {
    value: Mutex<Option<MeasurementSnapshot<Did>>>,
    attempts: AtomicU64,
    attempt_permits: Semaphore,
    failures_before_success: u64,
}

impl TransientStorage {
    fn new(failures_before_success: u64) -> Self {
        Self {
            state: Arc::new(TransientStorageState {
                value: Mutex::new(None),
                attempts: AtomicU64::new(0),
                attempt_permits: Semaphore::new(0),
                failures_before_success,
            }),
        }
    }

    async fn wait_for_attempts(&self, expected: u64) {
        while self.state.attempts.load(Ordering::SeqCst) < expected {
            let permit = self
                .state
                .attempt_permits
                .acquire()
                .await
                .unwrap_or_else(|error| panic!("attempt semaphore must remain open: {error}"));
            permit.forget();
        }
    }
}

#[async_trait]
impl KvStorageInterface<MeasurementSnapshot<Did>> for TransientStorage {
    async fn get(&self, _key: &str) -> rings_core::error::Result<Option<MeasurementSnapshot<Did>>> {
        Ok(lock_or_recover(&self.state.value).clone())
    }

    async fn put(
        &self,
        _key: &str,
        value: &MeasurementSnapshot<Did>,
    ) -> rings_core::error::Result<()> {
        let previous = self.state.attempts.fetch_add(1, Ordering::SeqCst);
        self.state.attempt_permits.add_permits(1);
        if previous < self.state.failures_before_success {
            return Err(CoreError::InvalidTransport);
        }
        *lock_or_recover(&self.state.value) = Some(value.clone());
        Ok(())
    }

    async fn get_all(&self) -> rings_core::error::Result<Vec<(String, MeasurementSnapshot<Did>)>> {
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

fn did() -> Did {
    Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0")
        .unwrap_or_else(|error| panic!("test DID must parse: {error}"))
}

async fn memory_measure(clock: Arc<ManualMeasureClock>) -> PeriodicMeasure {
    PeriodicMeasure::new_with_clock(Box::new(MemStorage::new()), clock)
        .await
        .unwrap_or_else(|error| panic!("memory measurement must initialize: {error}"))
}

#[cfg(not(all(feature = "browser", target_family = "wasm")))]
#[test]
fn native_construction_without_tokio_returns_typed_error() {
    let result = futures::executor::block_on(PeriodicMeasure::new(Box::new(MemStorage::new())));
    assert!(matches!(
        result,
        Err(MeasureRuntimeError::RuntimeUnavailable(_))
    ));
}

#[tokio::test]
async fn compatibility_counters_follow_the_live_epoch() {
    let clock = Arc::new(ManualMeasureClock::new(10));
    let measure = memory_measure(clock.clone()).await;
    measure.incr(did(), MeasureCounter::Sent).await;
    measure.incr(did(), MeasureCounter::Sent).await;
    assert_eq!(measure.get_count(did(), MeasureCounter::Sent).await, 2);
    clock.advance(RELIABILITY_WINDOW.get());
    assert_eq!(measure.get_count(did(), MeasureCounter::Sent).await, 0);
}

#[tokio::test]
async fn useful_bytes_produce_amule_credit_and_one_reliability_event() {
    let clock = Arc::new(ManualMeasureClock::new(10));
    let measure = memory_measure(clock).await;
    assert_eq!(
        measure
            .record(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Received {
                    useful_bytes: 2_000_000,
                },
            )
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
        FileStorage::new_with_cap_and_path(4096, path)
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
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 17 },
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must apply: {error}"));
    measure
        .flush()
        .await
        .unwrap_or_else(|error| panic!("measurement must flush: {error}"));
    drop(measure);

    let restored = PeriodicMeasure::new_with_clock(
        Box::new(
            FileStorage::new_with_cap_and_path(4096, path)
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
async fn quiet_query_paths_prune_records_at_the_retention_boundary() {
    #[derive(Clone, Copy, Debug)]
    enum QueryPath {
        Count,
        Single,
        Bulk,
        Page,
    }

    for path in [
        QueryPath::Count,
        QueryPath::Single,
        QueryPath::Bulk,
        QueryPath::Page,
    ] {
        let clock = Arc::new(ManualMeasureClock::new(10));
        let measure = memory_measure(clock.clone()).await;
        measure
            .record(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Connected,
            )
            .await
            .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));
        clock.set(10 + CreditPolicy::amule().retention_seconds());

        match path {
            QueryPath::Count => {
                assert_eq!(measure.get_count(did(), MeasureCounter::Connect).await, 0);
            }
            QueryPath::Single => {
                assert!(measure
                    .peer_measurement(did())
                    .await
                    .unwrap_or_else(|error| panic!("single projection must succeed: {error}"))
                    .is_none());
            }
            QueryPath::Bulk => {
                assert!(measure
                    .peer_measurements()
                    .await
                    .unwrap_or_else(|error| panic!("bulk projection must succeed: {error}"))
                    .is_empty());
            }
            QueryPath::Page => {
                assert!(measure
                    .peer_measurements_page(None, NonZeroUsize::MIN)
                    .await
                    .unwrap_or_else(|error| panic!("page projection must succeed: {error}"))
                    .measurements
                    .is_empty());
            }
        }

        assert!(
            lock_or_recover(&measure.state.runtime)
                .ledger
                .snapshot()
                .records
                .is_empty(),
            "{path:?} must remove the expired retained record"
        );
    }
}

#[tokio::test]
async fn early_maintenance_schedules_the_exact_retention_boundary() {
    let clock = Arc::new(ManualMeasureClock::new(10));
    let measure = memory_measure(clock.clone()).await;
    measure
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Connected,
        )
        .await
        .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));
    let retention = CreditPolicy::amule().retention_seconds();

    clock.set(10 + retention - 1);
    assert!(measure
        .peer_measurement(did())
        .await
        .unwrap_or_else(|error| panic!("pre-boundary projection must succeed: {error}"))
        .is_some());
    assert_eq!(
        lock_or_recover(&measure.state.runtime).next_prune_at,
        UnixTime::from_secs(10 + retention)
    );

    clock.set(10 + retention);
    assert!(measure
        .peer_measurement(did())
        .await
        .unwrap_or_else(|error| panic!("boundary projection must succeed: {error}"))
        .is_none());
}

#[tokio::test]
async fn query_pruning_persists_the_removed_record() {
    let storage = GatedStorage::default();
    let clock = Arc::new(ManualMeasureClock::new(10));
    let measure = PeriodicMeasure::new_with_clock(Box::new(storage.clone()), clock.clone())
        .await
        .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
    measure
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Connected,
        )
        .await
        .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));

    tokio::time::timeout(Duration::from_secs(1), storage.wait_for_writes(1))
        .await
        .unwrap_or_else(|_| panic!("initial snapshot write must start"));
    storage.release_first_write();
    measure
        .flush_with_timeout(Duration::from_secs(1))
        .await
        .unwrap_or_else(|error| panic!("initial snapshot must flush: {error}"));

    clock.set(10 + CreditPolicy::amule().retention_seconds());
    assert!(measure
        .peer_measurement(did())
        .await
        .unwrap_or_else(|error| panic!("expiry projection must succeed: {error}"))
        .is_none());
    measure
        .flush_with_timeout(Duration::from_secs(1))
        .await
        .unwrap_or_else(|error| panic!("pruned snapshot must flush: {error}"));

    let persisted = lock_or_recover(&storage.state.value)
        .clone()
        .unwrap_or_else(|| panic!("pruned snapshot must be persisted"));
    assert!(persisted.records.is_empty());
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

    let measure =
        PeriodicMeasure::new_with_clock(Box::new(storage), Arc::new(ManualMeasureClock::new(20)))
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
async fn startup_migrates_obsolete_reliability_window_without_losing_credit() {
    let storage = MemStorage::new();
    let old_policy = ReliabilityPolicy::new(60, 1, peer_quality_thresholds())
        .unwrap_or_else(|error| panic!("old fixture policy must be valid: {error}"));
    let mut ledger = MeasurementLedger::new();
    ledger
        .apply(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 41 },
            UnixTime::from_secs(120),
            old_policy,
        )
        .unwrap_or_else(|error| panic!("fixture event must apply: {error}"));
    storage
        .put(SNAPSHOT_KEY, &ledger.snapshot())
        .await
        .unwrap_or_else(|error| panic!("fixture snapshot must persist: {error}"));

    let measure =
        PeriodicMeasure::new_with_clock(Box::new(storage), Arc::new(ManualMeasureClock::new(120)))
            .await
            .unwrap_or_else(|error| panic!("window migration must not abort startup: {error}"));
    let projected = measure
        .peer_measurement(did())
        .await
        .unwrap_or_else(|error| panic!("projection must succeed after migration: {error}"))
        .unwrap_or_else(|| panic!("migrated record must remain visible"));

    assert_eq!(
        projected.credit.map(|credit| credit.bytes_sent_to_peer()),
        Some(41)
    );
    assert!(projected.evidence.is_unobserved());
    assert!(lock_or_recover(&measure.state.runtime).dirty);
}

#[tokio::test]
async fn live_query_reconciles_a_wall_clock_regression() {
    let clock = Arc::new(ManualMeasureClock::new(100));
    let measure = memory_measure(clock.clone()).await;
    measure
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Received { useful_bytes: 64 },
        )
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
            .record(
                did,
                Authentication::Authenticated,
                MeasurementEvent::Connected,
            )
            .await
            .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));
    }
    let limit = nonzero_usize(2);

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
        measure.record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 23 },
        ),
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
async fn cancelled_snapshot_write_keeps_runtime_dirty() {
    let storage = GatedStorage::default();
    let measure = PeriodicMeasure::new_with_clock(
        Box::new(storage.clone()),
        Arc::new(ManualMeasureClock::new(10)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
    {
        let mut runtime = lock_or_recover(&measure.state.runtime);
        runtime
            .ledger
            .apply(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(10),
                reliability_policy(),
            )
            .unwrap_or_else(|error| panic!("fixture measurement must apply: {error}"));
        mark_runtime_dirty(&mut runtime);
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(10), flush_state(&measure.state))
            .await
            .is_err(),
        "the first gated write must be cancelled by the deadline"
    );
    assert!(lock_or_recover(&measure.state.runtime).dirty);

    storage.release_first_write();
    flush_state(&measure.state)
        .await
        .unwrap_or_else(|error| panic!("a later flush must persist cancelled work: {error}"));
    assert!(!lock_or_recover(&measure.state.runtime).dirty);
    let persisted = lock_or_recover(&storage.state.value)
        .clone()
        .unwrap_or_else(|| panic!("the replacement snapshot must be persisted"));
    assert_eq!(persisted.records.len(), 1);
}

#[tokio::test]
async fn cancelled_public_flush_keeps_the_owned_write_ordered() {
    let storage = GatedStorage::default();
    let measure = PeriodicMeasure::new_with_clock(
        Box::new(storage.clone()),
        Arc::new(ManualMeasureClock::new(10)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
    {
        let mut runtime = lock_or_recover(&measure.state.runtime);
        runtime
            .ledger
            .apply(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(10),
                reliability_policy(),
            )
            .unwrap_or_else(|error| panic!("first measurement must apply: {error}"));
        mark_runtime_dirty(&mut runtime);
    }

    {
        let first_flush = measure.flush();
        tokio::pin!(first_flush);
        tokio::select! {
            () = storage.wait_for_writes(1) => {}
            result = &mut first_flush => {
                panic!("gated flush unexpectedly completed: {result:?}");
            }
        }
    }
    {
        let mut runtime = lock_or_recover(&measure.state.runtime);
        runtime
            .ledger
            .apply(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(10),
                reliability_policy(),
            )
            .unwrap_or_else(|error| panic!("second measurement must apply: {error}"));
        mark_runtime_dirty(&mut runtime);
    }

    let second_flush = measure.flush();
    tokio::pin!(second_flush);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), second_flush.as_mut())
            .await
            .is_err(),
        "the next flush must remain ordered behind the owned first write"
    );
    assert_eq!(storage.state.writes.load(Ordering::SeqCst), 1);
    storage.release_first_write();
    second_flush
        .await
        .unwrap_or_else(|error| panic!("second flush must persist newer state: {error}"));

    assert_eq!(storage.state.writes.load(Ordering::SeqCst), 2);
    let persisted = lock_or_recover(&storage.state.value)
        .clone()
        .unwrap_or_else(|| panic!("newest ordered snapshot must be persisted"));
    assert_eq!(
        persisted.records[0]
            .record
            .reliability()
            .stored_evidence()
            .connected,
        2
    );
}

#[tokio::test]
async fn persistence_worker_retries_a_transient_storage_failure() {
    let storage = TransientStorage::new(1);
    let measure = PeriodicMeasure::new_with_clock(
        Box::new(storage.clone()),
        Arc::new(ManualMeasureClock::new(10)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
    measure
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 31 },
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must apply: {error}"));

    tokio::time::timeout(Duration::from_secs(1), storage.wait_for_attempts(2))
        .await
        .unwrap_or_else(|_| panic!("persistence worker must retry without a second event"));
    let persisted = lock_or_recover(&storage.state.value)
        .clone()
        .unwrap_or_else(|| panic!("retry must persist the pending snapshot"));
    assert_eq!(persisted.records.len(), 1);
    assert!(!lock_or_recover(&measure.state.runtime).dirty);
}

#[tokio::test]
async fn closed_worker_retries_transient_final_write_failures() {
    let storage = TransientStorage::new(2);
    let measure = PeriodicMeasure::new_with_clock(
        Box::new(storage.clone()),
        Arc::new(ManualMeasureClock::new(10)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
    measure
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 37 },
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must apply: {error}"));
    drop(measure);

    tokio::time::timeout(Duration::from_secs(1), storage.wait_for_attempts(3))
        .await
        .unwrap_or_else(|_| panic!("closed worker must make bounded final retries"));
    let persisted = lock_or_recover(&storage.state.value)
        .clone()
        .unwrap_or_else(|| panic!("final retry must persist the pending snapshot"));
    assert_eq!(persisted.records.len(), 1);
}

#[tokio::test]
async fn persistence_enforces_a_minimum_interval_between_full_snapshots() {
    let storage = GatedStorage::default();
    let clock = Arc::new(ManualMeasureClock::new(10));
    let measure = PeriodicMeasure::new_with_clock(Box::new(storage.clone()), clock)
        .await
        .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
    measure
        .record(
            did(),
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 1 },
        )
        .await
        .unwrap_or_else(|error| panic!("first measurement must apply: {error}"));
    tokio::time::timeout(Duration::from_secs(1), storage.wait_for_writes(1))
        .await
        .unwrap_or_else(|_| panic!("first snapshot write must start"));

    for _ in 0..32 {
        measure
            .record(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Sent { useful_bytes: 1 },
            )
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
            .record(
                did(),
                Authentication::Authenticated,
                MeasurementEvent::Received { useful_bytes: 29 },
            )
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
