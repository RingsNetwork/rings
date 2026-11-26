use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::delegation::DelegateeKey;
use rings_core::error::Error as CoreError;
use rings_core::message::MessageSigner;
use rings_core::message::ProvisionalEpoch;
use rings_core::message::ProvisionalServiceClaim;
use rings_core::message::ProvisionalServiceReceipt;
use rings_core::storage::file::FileStorage;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::MemStorage;
use rings_core::storage::RecordAuthority;

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

struct FailingEvidenceStorage;

#[async_trait]
impl KvStorageInterface<EvidenceStorageEntry<Did>> for FailingEvidenceStorage {
    async fn get(
        &self,
        _key: &str,
    ) -> rings_core::error::Result<Option<EvidenceStorageEntry<Did>>> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &str,
        _value: &EvidenceStorageEntry<Did>,
    ) -> rings_core::error::Result<()> {
        Err(CoreError::ServiceIOError(std::io::Error::other(
            "store unavailable",
        )))
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

#[derive(Clone, Default)]
struct MapEvidenceStorage {
    entries: Arc<std::sync::Mutex<BTreeMap<String, EvidenceStorageEntry<Did>>>>,
    puts: Arc<AtomicU64>,
    removes: Arc<AtomicU64>,
}

impl MapEvidenceStorage {
    fn snapshot_entries(&self) -> BTreeMap<String, EvidenceStorageEntry<Did>> {
        lock_or_recover(&self.entries).clone()
    }
}

#[async_trait]
impl KvStorageInterface<EvidenceStorageEntry<Did>> for MapEvidenceStorage {
    async fn get(&self, key: &str) -> rings_core::error::Result<Option<EvidenceStorageEntry<Did>>> {
        Ok(lock_or_recover(&self.entries).get(key).cloned())
    }

    async fn put(
        &self,
        key: &str,
        value: &EvidenceStorageEntry<Did>,
    ) -> rings_core::error::Result<()> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        lock_or_recover(&self.entries).insert(key.to_string(), value.clone());
        Ok(())
    }

    async fn get_all(&self) -> rings_core::error::Result<Vec<(String, EvidenceStorageEntry<Did>)>> {
        Ok(lock_or_recover(&self.entries)
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    async fn remove(&self, key: &str) -> rings_core::error::Result<()> {
        self.removes.fetch_add(1, Ordering::SeqCst);
        lock_or_recover(&self.entries).remove(key);
        Ok(())
    }

    async fn clear(&self) -> rings_core::error::Result<()> {
        lock_or_recover(&self.entries).clear();
        Ok(())
    }

    async fn count(&self) -> rings_core::error::Result<u32> {
        Ok(u32::try_from(lock_or_recover(&self.entries).len()).unwrap_or(u32::MAX))
    }
}

impl ManualMeasureClock {
    const fn new(now: u64) -> Self {
        Self {
            now: AtomicU64::new(now),
        }
    }
}

impl MeasureClock for ManualMeasureClock {
    fn now(&self) -> UnixTime {
        UnixTime::from_secs(self.now.load(Ordering::SeqCst))
    }
}

fn make_evidence_record_for_provider(
    provider: &DelegateeKey,
    observed_at_seconds: u64,
    tag: u8,
) -> ProvisionalEvidenceRecord<Did> {
    let beneficiary = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("beneficiary delegation must build: {error}"));
    let claim = ProvisionalServiceClaim::probe(
        7,
        provider.delegator_did(),
        beneficiary.delegator_did(),
        ProvisionalEpoch::from_unix_seconds(observed_at_seconds),
        *rings_core::ecc::SecretKey::random().ser(),
        [tag.wrapping_add(1); 32],
        [tag.wrapping_add(2); 32],
    );
    let provider_attestation = claim
        .sign_provider(MessageSigner::new(provider, 7))
        .unwrap_or_else(|error| panic!("provider attestation must sign: {error}"));
    let beneficiary_attestation = claim
        .sign_beneficiary(MessageSigner::new(&beneficiary, 7))
        .unwrap_or_else(|error| panic!("beneficiary attestation must sign: {error}"));
    let receipt = ProvisionalServiceReceipt::new(
        claim.clone(),
        provider_attestation,
        beneficiary_attestation,
    )
    .unwrap_or_else(|error| panic!("receipt must assemble: {error}"));
    ProvisionalEvidenceRecord::new(
        rings_measure::EvidenceAccountPair::new(claim.provider_account, claim.beneficiary_account),
        rings_measure::EvidenceFreshnessKey::new(
            claim.network_id,
            claim.beneficiary_account,
            claim.epoch.slot,
            claim.nonce,
        ),
        EvidenceDigest::new(
            receipt
                .digest()
                .unwrap_or_else(|error| panic!("receipt must hash: {error}"))
                .into_bytes(),
        ),
        receipt
            .canonical_bytes()
            .unwrap_or_else(|error| panic!("receipt must encode: {error}")),
        UnixTime::from_secs(observed_at_seconds),
        claim.epoch.slot.saturating_sub(1),
    )
}

fn provisional_evidence_fixture(
    tag: u8,
) -> (ProvisionalEvidenceRecord<Did>, EvidenceCollectorIdentity) {
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let observed_at_ms = rings_core::utils::get_epoch_ms();
    let observed_at_seconds = u64::try_from(observed_at_ms / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let record = make_evidence_record_for_provider(&provider, observed_at_seconds, tag);
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    (record, collector)
}

#[test]
fn persisted_evidence_revalidates_observation_time_and_isolates_invalid_neighbors() {
    let (valid, collector) = provisional_evidence_fixture(4);
    let (source, _) = provisional_evidence_fixture(8);
    let future_observation =
        UnixTime::from_secs(source.observed_at().as_secs().saturating_add(900));
    let malformed = ProvisionalEvidenceRecord::new(
        *source.pair(),
        *source.freshness(),
        source.digest(),
        source.canonical_receipt().to_vec(),
        future_observation,
        source.replay_floor(),
    );
    assert!(valid_persisted_evidence(&valid, collector));
    assert!(!valid_persisted_evidence(&malformed, collector));

    let mut snapshot_store = ProvisionalEvidenceStore::new(EvidenceLimits::default());
    snapshot_store
        .admit(malformed)
        .unwrap_or_else(|error| panic!("malformed fixture must enter the raw snapshot: {error}"));
    snapshot_store
        .admit(valid.clone())
        .unwrap_or_else(|error| panic!("valid fixture must enter the raw snapshot: {error}"));
    let (restored, report) = ProvisionalEvidenceStore::from_snapshot_with_validator(
        snapshot_store.snapshot(),
        EvidenceLimits::default(),
        |record| valid_persisted_evidence(record, collector),
        |marker| valid_persisted_replay_marker(marker, collector),
    )
    .unwrap_or_else(|error| panic!("snapshot must restore valid neighbors: {error}"));

    assert_eq!(report.rejected_records(), 1);
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored
            .page(None, nonzero_usize(2))
            .records()
            .first()
            .map(ProvisionalEvidenceRecord::digest),
        Some(valid.digest())
    );
}

#[test]
fn persisted_evidence_is_bound_to_the_runtime_network_and_provider() {
    let (record, collector) = provisional_evidence_fixture(4);
    let foreign_network = EvidenceCollectorIdentity::new(
        collector.network_id.saturating_add(1),
        collector.provider_account,
    );
    let foreign_provider = EvidenceCollectorIdentity::new(
        collector.network_id,
        collector.provider_account + Did::from(1_u32),
    );

    assert!(valid_persisted_evidence(&record, collector));
    assert!(!valid_persisted_evidence(&record, foreign_network));
    assert!(!valid_persisted_evidence(&record, foreign_provider));
}

#[tokio::test]
async fn startup_rejects_foreign_network_and_provider_snapshot_records() {
    let (record, collector) = provisional_evidence_fixture(4);
    let rejected_collectors = [
        EvidenceCollectorIdentity::new(
            collector.network_id.saturating_add(1),
            collector.provider_account,
        ),
        EvidenceCollectorIdentity::new(
            collector.network_id,
            collector.provider_account + Did::from(1_u32),
        ),
    ];

    for rejected_collector in rejected_collectors {
        let evidence_storage = MemStorage::new();
        let mut snapshot_store = ProvisionalEvidenceStore::new(EvidenceLimits::default());
        snapshot_store
            .admit(record.clone())
            .unwrap_or_else(|error| panic!("foreign fixture must enter its raw snapshot: {error}"));
        evidence_storage
            .put(EVIDENCE_SNAPSHOT_KEY, &EvidenceStorageEntry::Snapshot {
                compacted_through_sequence: 0,
                snapshot: snapshot_store.snapshot(),
            })
            .await
            .unwrap_or_else(|error| panic!("foreign snapshot must persist: {error}"));

        let measure = PeriodicMeasure::new_with_clock_and_evidence(
            Box::new(MemStorage::new()),
            Box::new(evidence_storage),
            Some(rejected_collector),
            Arc::new(ManualMeasureClock::new(10)),
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must reconcile foreign evidence: {error}"));
        let page = measure
            .provisional_evidence_page(None, nonzero_usize(1))
            .await
            .unwrap_or_else(|error| panic!("reconciled evidence must project: {error}"));

        assert!(page.records().is_empty());
        assert_eq!(
            measure
                .provisional_evidence_counters()
                .await
                .rejected_records(),
            1
        );
        assert_eq!(
            measure
                .provisional_evidence_counters()
                .await
                .rejected_replay_markers(),
            1
        );
    }
}

#[tokio::test]
async fn failed_durable_admission_rolls_back_receipt_and_replay_marker() {
    let clock = Arc::new(ManualMeasureClock::new(10));
    let (record, collector) = provisional_evidence_fixture(4);
    let measure = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(FailingEvidenceStorage),
        Some(collector),
        clock,
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

    assert_eq!(
        measure.admit_provisional_evidence(record).await,
        Err(EvidenceError::PersistenceUnavailable)
    );
    assert!(measure
        .provisional_evidence_page(None, nonzero_usize(1))
        .await
        .unwrap_or_else(|error| panic!("rolled-back evidence must project: {error}"))
        .records()
        .is_empty());
    assert_eq!(
        measure.provisional_evidence_counters().await,
        EvidenceCounters::default()
    );
}

#[tokio::test]
async fn missing_durable_evidence_backend_never_returns_admitted() {
    let (record, _) = provisional_evidence_fixture(5);
    let measure = PeriodicMeasure::new(Box::new(MemStorage::new()))
        .await
        .unwrap_or_else(|error| panic!("measurement-only runtime must initialize: {error}"));

    assert_eq!(
        measure.admit_provisional_evidence(record).await,
        Err(EvidenceError::PersistenceUnavailable)
    );
    assert!(measure
        .provisional_evidence_page(None, nonzero_usize(1))
        .await
        .unwrap_or_else(|error| panic!("rolled-back evidence must project: {error}"))
        .records()
        .is_empty());
}

/// Admit one provisional evidence record into fresh measure and evidence stores at the two
/// paths, then crash: the instance's runtime is dropped before the instance, so its persistence
/// worker is cancelled before any final flush, and the stores are released with the instance,
/// as a dead process releases them (one store directory has one writer, #1039).
fn admit_then_crash(
    measure_path: &'static str,
    evidence_path: &'static str,
    clock: Arc<ManualMeasureClock>,
) -> EvidenceCollectorIdentity {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new()
            .unwrap_or_else(|error| panic!("crashing runtime must start: {error}"));
        let (measure, collector) = runtime.block_on(async move {
            let measure_storage: MeasureStorage = Box::new(
                FileStorage::new_with_cap_and_path(1024 * 1024, measure_path)
                    .await
                    .unwrap_or_else(|error| panic!("measurement storage must open: {error}")),
            );
            let evidence_storage: EvidenceStorage = Box::new(
                FileStorage::new_with_cap_and_path(1024 * 1024, evidence_path)
                    .await
                    .unwrap_or_else(|error| panic!("evidence storage must open: {error}")),
            );
            measure_storage
                .clear()
                .await
                .unwrap_or_else(|error| panic!("measurement storage must clear: {error}"));
            evidence_storage
                .clear()
                .await
                .unwrap_or_else(|error| panic!("evidence storage must clear: {error}"));
            let (record, collector) = provisional_evidence_fixture(4);
            let measure = PeriodicMeasure::new_with_clock_and_evidence(
                measure_storage,
                evidence_storage,
                Some(collector),
                clock.clone(),
            )
            .await
            .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));
            assert!(matches!(
                measure
                    .admit_provisional_evidence(record)
                    .await
                    .map(|report| report.admission()),
                Ok(rings_measure::EvidenceAdmission::Admitted)
            ));
            (measure, collector)
        });
        drop(runtime);
        drop(measure);
        collector
    })
    .join()
    .unwrap_or_else(|_| panic!("the crashing instance must run"))
}

#[tokio::test]
async fn successful_admission_survives_native_restart_without_flush() {
    let measure_path = "tmp/measure_with_evidence_test_db";
    let evidence_path = "tmp/provisional_evidence_test_db";
    let clock = Arc::new(ManualMeasureClock::new(10));
    let collector = admit_then_crash(measure_path, evidence_path, clock.clone());

    let restored = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(
            FileStorage::new_with_cap_and_path(1024 * 1024, measure_path)
                .await
                .unwrap_or_else(|error| panic!("measurement storage must reopen: {error}")),
        ),
        Box::new(
            FileStorage::new_with_cap_and_path(1024 * 1024, evidence_path)
                .await
                .unwrap_or_else(|error| panic!("evidence storage must reopen: {error}")),
        ),
        Some(collector),
        clock,
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must restore: {error}"));
    let page = restored
        .provisional_evidence_page(None, nonzero_usize(4))
        .await
        .unwrap_or_else(|error| panic!("evidence page must project: {error}"));
    assert_eq!(page.records().len(), 1);
}

#[tokio::test]
async fn evidence_reads_do_not_block_or_observe_uncommitted_state_during_slow_put() {
    use crate::test_support::TestTask;

    #[derive(Clone)]
    struct BlockingEvidenceStorage {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl KvStorageInterface<EvidenceStorageEntry<Did>> for BlockingEvidenceStorage {
        async fn get(
            &self,
            _key: &str,
        ) -> rings_core::error::Result<Option<EvidenceStorageEntry<Did>>> {
            Ok(None)
        }

        async fn put(
            &self,
            _key: &str,
            _value: &EvidenceStorageEntry<Did>,
        ) -> rings_core::error::Result<()> {
            self.entered.notify_waiters();
            self.release.notified().await;
            Ok(())
        }

        async fn get_all(
            &self,
        ) -> rings_core::error::Result<Vec<(String, EvidenceStorageEntry<Did>)>> {
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

    let (record, collector) = provisional_evidence_fixture(9);
    let blocking = BlockingEvidenceStorage {
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let measure = Arc::new(
        PeriodicMeasure::new_with_clock_and_evidence(
            Box::new(MemStorage::new()),
            Box::new(blocking.clone()),
            Some(collector),
            Arc::new(ManualMeasureClock::new(10)),
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must initialize: {error}")),
    );

    let entered = blocking.entered.notified();
    let admitting_measure = Arc::clone(&measure);
    let admission =
        TestTask::spawn(async move { admitting_measure.admit_provisional_evidence(record).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered)
        .await
        .unwrap_or_else(|_| panic!("admission must enter evidence_storage.put"));

    // Read queries complete immediately without waiting on evidence_turns and still see 0
    // committed records until the durable write finishes.
    let in_flight_page = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        measure.provisional_evidence_page(None, nonzero_usize(4)),
    )
    .await
    .unwrap_or_else(|_| panic!("evidence page query must not block on storage put"))
    .unwrap_or_else(|error| panic!("evidence page must project: {error}"));
    assert!(in_flight_page.records().is_empty());

    blocking.release.notify_waiters();
    let report = admission
        .await
        .unwrap_or_else(|error| panic!("admission task must join: {error}"))
        .unwrap_or_else(|error| panic!("admission must commit: {error}"));
    assert_eq!(
        report.admission(),
        rings_measure::EvidenceAdmission::Admitted
    );

    let committed_page = measure
        .provisional_evidence_page(None, nonzero_usize(4))
        .await
        .unwrap_or_else(|error| panic!("committed page must project: {error}"));
    assert_eq!(committed_page.records().len(), 1);
}

#[tokio::test]
async fn test_cancelled_committed_admission_survives_subsequent_failed_admission_rollback() {
    use std::sync::atomic::AtomicBool;

    use crate::test_support::TestTask;

    #[derive(Clone)]
    struct ControlledEvidenceStorage {
        entries: Arc<std::sync::Mutex<BTreeMap<String, EvidenceStorageEntry<Did>>>>,
        park_after_commit: Arc<AtomicBool>,
        fail_next_put: Arc<AtomicBool>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl KvStorageInterface<EvidenceStorageEntry<Did>> for ControlledEvidenceStorage {
        async fn get(
            &self,
            key: &str,
        ) -> rings_core::error::Result<Option<EvidenceStorageEntry<Did>>> {
            Ok(lock_or_recover(&self.entries).get(key).cloned())
        }

        async fn put(
            &self,
            key: &str,
            value: &EvidenceStorageEntry<Did>,
        ) -> rings_core::error::Result<()> {
            if self.fail_next_put.swap(false, Ordering::SeqCst) {
                return Err(CoreError::ServiceIOError(std::io::Error::other(
                    "store unavailable",
                )));
            }
            lock_or_recover(&self.entries).insert(key.to_string(), value.clone());
            if self.park_after_commit.swap(false, Ordering::SeqCst) {
                self.entered.notify_waiters();
                self.release.notified().await;
            }
            Ok(())
        }

        async fn get_all(
            &self,
        ) -> rings_core::error::Result<Vec<(String, EvidenceStorageEntry<Did>)>> {
            Ok(lock_or_recover(&self.entries)
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        }

        async fn remove(&self, key: &str) -> rings_core::error::Result<()> {
            lock_or_recover(&self.entries).remove(key);
            Ok(())
        }

        async fn clear(&self) -> rings_core::error::Result<()> {
            lock_or_recover(&self.entries).clear();
            Ok(())
        }

        async fn count(&self) -> rings_core::error::Result<u32> {
            Ok(u32::try_from(lock_or_recover(&self.entries).len()).unwrap_or(u32::MAX))
        }
    }

    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let record_a = make_evidence_record_for_provider(&provider, 10, 11);
    let record_b = make_evidence_record_for_provider(&provider, 10, 21);

    let storage = ControlledEvidenceStorage {
        entries: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        park_after_commit: Arc::new(AtomicBool::new(true)),
        fail_next_put: Arc::new(AtomicBool::new(false)),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let measure = Arc::new(
        PeriodicMeasure::new_with_clock_and_evidence(
            Box::new(MemStorage::new()),
            Box::new(storage.clone()),
            Some(collector),
            Arc::new(ManualMeasureClock::new(10)),
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must initialize: {error}")),
    );

    // Step 1: start admission A, wait until its store write has committed while its future is
    // still parked, then cancel A.
    let entered = storage.entered.notified();
    let admitting_a = Arc::clone(&measure);
    let record_a_first = record_a.clone();
    let task_a =
        TestTask::spawn(
            async move { admitting_a.admit_provisional_evidence(record_a_first).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(1), entered)
        .await
        .unwrap_or_else(|_| panic!("admission A must commit to store and park"));
    drop(task_a);
    // Allow the cancelled task to drop its `evidence_turns` guard.
    tokio::task::yield_now().await;

    // Step 2: run admission B whose store write fails, triggering rollback.
    storage.fail_next_put.store(true, Ordering::SeqCst);
    assert_eq!(
        measure.admit_provisional_evidence(record_b).await,
        Err(EvidenceError::PersistenceUnavailable)
    );

    // Step 3: retry admission A. Because B's rollback preserved A's committed state, A's replay
    // marker and receipt remain present so the retry reports `Duplicate` rather than `Admitted`.
    let retry_report = measure
        .admit_provisional_evidence(record_a.clone())
        .await
        .unwrap_or_else(|error| panic!("retry of admission A must succeed: {error}"));
    assert_eq!(
        retry_report.admission(),
        rings_measure::EvidenceAdmission::Duplicate
    );
    let page = measure
        .provisional_evidence_page(None, nonzero_usize(4))
        .await
        .unwrap_or_else(|error| panic!("committed page must project: {error}"));
    assert_eq!(page.records().len(), 1);
    assert_eq!(
        page.records().first().map(|r| r.digest()),
        Some(record_a.digest())
    );
}

#[tokio::test]
async fn evidence_admission_does_not_block_on_slow_ledger_write_or_dirty_ledger() {
    use std::sync::atomic::AtomicBool;

    use crate::test_support::TestTask;

    #[derive(Clone)]
    struct GatedLedgerStorage {
        gate_next: Arc<AtomicBool>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        writes: Arc<AtomicU64>,
    }

    #[async_trait]
    impl KvStorageInterface<MeasurementSnapshot<Did>> for GatedLedgerStorage {
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
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.gate_next.swap(false, Ordering::SeqCst) {
                self.entered.notify_waiters();
                self.release.notified().await;
            }
            Ok(())
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

    let (record, collector) = provisional_evidence_fixture(12);
    let ledger_storage = GatedLedgerStorage {
        gate_next: Arc::new(AtomicBool::new(true)),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        writes: Arc::new(AtomicU64::new(0)),
    };
    let evidence_storage = MapEvidenceStorage::default();
    let measure = Arc::new(
        PeriodicMeasure::new_with_clock_and_evidence(
            Box::new(ledger_storage.clone()),
            Box::new(evidence_storage.clone()),
            Some(collector),
            Arc::new(ManualMeasureClock::new(10)),
        )
        .await
        .unwrap_or_else(|error| panic!("measurement must initialize: {error}")),
    );

    let entered = ledger_storage.entered.notified();
    let flushing_measure = Arc::clone(&measure);
    let flush_task = TestTask::spawn(async move { flushing_measure.flush().await });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered)
        .await
        .unwrap_or_else(|_| panic!("ledger flush must park inside storage.put"));

    // Provisional evidence admission uses its own WriteTurnQueue and must complete immediately
    // without blocking on the slow ledger write or dirtying the ledger snapshot.
    let report = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        measure.admit_provisional_evidence(record),
    )
    .await
    .unwrap_or_else(|_| panic!("evidence admission must not block behind ledger write"))
    .unwrap_or_else(|error| panic!("evidence admission must succeed: {error}"));
    assert_eq!(
        report.admission(),
        rings_measure::EvidenceAdmission::Admitted
    );
    assert_eq!(evidence_storage.puts.load(Ordering::SeqCst), 1);
    assert!(!lock_or_recover(&measure.state.runtime).dirty);
    assert!(!lock_or_recover(&measure.state.runtime).evidence_dirty);

    ledger_storage.release.notify_waiters();
    flush_task
        .await
        .unwrap_or_else(|error| panic!("flush task must join: {error}"))
        .unwrap_or_else(|error| panic!("ledger flush must finish: {error}"));
    assert_eq!(ledger_storage.writes.load(Ordering::SeqCst), 1);
    // Explicit `flush()` compacts pending incremental deltas into a single Snapshot checkpoint.
    assert_eq!(evidence_storage.puts.load(Ordering::SeqCst), 2);

    // Mutate only the measurement ledger and run persist_pending_once; clean evidence_storage
    // must not be rewritten.
    let peer: Did = rings_core::ecc::SecretKey::random().address().into();
    measure
        .record(
            peer,
            rings_measure::Authentication::Authenticated,
            rings_measure::MeasurementEvent::Sent { useful_bytes: 19 },
        )
        .await
        .unwrap_or_else(|error| panic!("ledger record must apply: {error}"));
    persist_pending_once(&measure.state)
        .await
        .unwrap_or_else(|error| panic!("ledger-only persist must succeed: {error}"));
    assert_eq!(ledger_storage.writes.load(Ordering::SeqCst), 2);
    assert_eq!(evidence_storage.puts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn incremental_admissions_write_deltas_and_survive_restart_without_flush() {
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let now_secs = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let r1 = make_evidence_record_for_provider(&provider, now_secs, 1);
    let r2 = make_evidence_record_for_provider(&provider, now_secs, 2);
    let r3 = make_evidence_record_for_provider(&provider, now_secs, 3);
    let r4 = make_evidence_record_for_provider(&provider, now_secs, 4);

    let evidence_storage = MapEvidenceStorage::default();
    let measure = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

    for record in [r1.clone(), r2.clone(), r3.clone()] {
        let report = measure
            .admit_provisional_evidence(record)
            .await
            .unwrap_or_else(|error| panic!("delta admission must succeed: {error}"));
        assert_eq!(report.admission(), EvidenceAdmission::Admitted);
    }

    let entries = evidence_storage.snapshot_entries();
    assert!(!entries.contains_key(EVIDENCE_SNAPSHOT_KEY));
    assert_eq!(entries.len(), 3);
    assert_eq!(entries.keys().cloned().collect::<Vec<_>>(), vec![
        evidence_delta_key(1),
        evidence_delta_key(2),
        evidence_delta_key(3),
    ]);

    // Simulate crash recovery without calling `flush()` or `close()`: drop the live instance and
    // reopen from the delta entries alone.
    drop(measure);
    let restored = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must restore from deltas: {error}"));

    let page = restored
        .provisional_evidence_page(None, nonzero_usize(10))
        .await
        .unwrap_or_else(|error| panic!("restored evidence page must project: {error}"));
    let mut restored_digests: Vec<_> = page.records().iter().map(|r| r.digest()).collect();
    restored_digests.sort();
    let mut expected_digests = vec![r1.digest(), r2.digest(), r3.digest()];
    expected_digests.sort();
    assert_eq!(restored_digests, expected_digests);
    assert_eq!(restored.provisional_evidence_counters().await.admitted(), 3);

    // Next admission after restart must continue monotonically at sequence 4 (never overwrite 1).
    let report4 = restored
        .admit_provisional_evidence(r4)
        .await
        .unwrap_or_else(|error| panic!("post-restart delta admission must succeed: {error}"));
    assert_eq!(report4.admission(), EvidenceAdmission::Admitted);
    let after_restart_entries = evidence_storage.snapshot_entries();
    assert!(after_restart_entries.contains_key(&evidence_delta_key(4)));
    assert_eq!(after_restart_entries.len(), 4);
}

#[tokio::test]
async fn delta_compaction_triggers_at_threshold_and_prunes_compacted_keys() {
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let now_secs = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let records: Vec<_> = (1..=5_u8)
        .map(|tag| make_evidence_record_for_provider(&provider, now_secs, tag))
        .collect();

    let evidence_storage = MapEvidenceStorage::default();
    let measure = PeriodicMeasure::new_with_clock_evidence_and_threshold(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
        3,
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

    // Admit 3 records to hit threshold = 3.
    for record in &records[..3] {
        assert_eq!(
            measure
                .admit_provisional_evidence(record.clone())
                .await
                .unwrap_or_else(|error| panic!("admission must commit: {error}"))
                .admission(),
            EvidenceAdmission::Admitted
        );
    }
    assert!(lock_or_recover(&measure.state.runtime).evidence_dirty);

    // Next admission immediately compacts dirty delta log into a Snapshot checkpoint and prunes
    // the compacted delta keys (1..=3).
    assert_eq!(
        measure
            .admit_provisional_evidence(records[3].clone())
            .await
            .unwrap_or_else(|error| panic!("compacting admission must commit: {error}"))
            .admission(),
        EvidenceAdmission::Admitted
    );

    let entries = evidence_storage.snapshot_entries();
    assert_eq!(entries.len(), 1);
    match entries.get(EVIDENCE_SNAPSHOT_KEY) {
        Some(EvidenceStorageEntry::Snapshot {
            compacted_through_sequence,
            snapshot,
        }) => {
            assert_eq!(*compacted_through_sequence, 3);
            assert_eq!(snapshot.records.len(), 4);
        }
        other => panic!("expected compacted Snapshot entry, got {other:?}"),
    }

    // Reopening from the compacted snapshot preserves sequence monotonicity so the next admission
    // writes at sequence >= 4 rather than resetting to 1.
    drop(measure);
    let reopened = PeriodicMeasure::new_with_clock_evidence_and_threshold(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
        3,
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must reopen cleanly: {error}"));
    assert_eq!(
        reopened
            .admit_provisional_evidence(records[4].clone())
            .await
            .unwrap_or_else(|error| panic!("post-compaction admission must succeed: {error}"))
            .admission(),
        EvidenceAdmission::Admitted
    );
    let after = evidence_storage.snapshot_entries();
    assert!(after.contains_key(EVIDENCE_SNAPSHOT_KEY));
    assert!(after.contains_key(&evidence_delta_key(4)));
    assert_eq!(reopened.provisional_evidence_counters().await.admitted(), 5);
}

#[tokio::test]
async fn startup_skips_stale_deltas_left_after_compaction_crash() {
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let now_secs = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let r1 = make_evidence_record_for_provider(&provider, now_secs, 1);
    let r2 = make_evidence_record_for_provider(&provider, now_secs, 2);
    let r3 = make_evidence_record_for_provider(&provider, now_secs, 3);

    let mut base_store = ProvisionalEvidenceStore::new(EvidenceLimits::default());
    base_store
        .admit(r1.clone())
        .unwrap_or_else(|error| panic!("r1 must enter base snapshot: {error}"));
    base_store
        .admit(r2.clone())
        .unwrap_or_else(|error| panic!("r2 must enter base snapshot: {error}"));

    let evidence_storage = MapEvidenceStorage::default();
    evidence_storage
        .put(EVIDENCE_SNAPSHOT_KEY, &EvidenceStorageEntry::Snapshot {
            compacted_through_sequence: 2,
            snapshot: base_store.snapshot(),
        })
        .await
        .unwrap_or_else(|error| panic!("snapshot must seed: {error}"));
    for (seq, rec) in [(1_u64, r1), (2_u64, r2), (3_u64, r3)] {
        evidence_storage
            .put(
                &evidence_delta_key(seq),
                &EvidenceStorageEntry::Delta(EvidenceAdmissionDelta {
                    schema_version: EVIDENCE_SNAPSHOT_VERSION,
                    sequence: seq,
                    record: rec,
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("delta must seed: {error}"));
    }

    let restored = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("restore with stale deltas must succeed: {error}"));

    let counters = restored.provisional_evidence_counters().await;
    assert_eq!(counters.admitted(), 3);
    assert_eq!(
        counters.duplicates(),
        0,
        "stale deltas <= compacted_through_sequence must not be replayed as duplicates"
    );
    assert!(lock_or_recover(&restored.state.runtime).evidence_dirty);

    // Running persistence compacts delta 3 and removes stale delta keys 1, 2, 3.
    persist_pending_once(&restored.state)
        .await
        .unwrap_or_else(|error| panic!("cleanup persist must succeed: {error}"));
    let cleaned = evidence_storage.snapshot_entries();
    assert_eq!(cleaned.len(), 1);
    match cleaned.get(EVIDENCE_SNAPSHOT_KEY) {
        Some(EvidenceStorageEntry::Snapshot {
            compacted_through_sequence,
            snapshot,
        }) => {
            assert_eq!(*compacted_through_sequence, 3);
            assert_eq!(snapshot.records.len(), 3);
        }
        other => panic!("expected compacted snapshot after cleanup, got {other:?}"),
    }
}

#[tokio::test]
async fn startup_isolates_foreign_or_corrupt_delta_while_preserving_valid_neighbors() {
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let foreign_provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("foreign provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let now_secs = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let r1 = make_evidence_record_for_provider(&provider, now_secs, 1);
    let foreign_r2 = make_evidence_record_for_provider(&foreign_provider, now_secs, 2);
    let r3 = make_evidence_record_for_provider(&provider, now_secs, 3);

    let evidence_storage = MapEvidenceStorage::default();
    for (seq, rec) in [
        (1_u64, r1.clone()),
        (2_u64, foreign_r2),
        (3_u64, r3.clone()),
    ] {
        evidence_storage
            .put(
                &evidence_delta_key(seq),
                &EvidenceStorageEntry::Delta(EvidenceAdmissionDelta {
                    schema_version: EVIDENCE_SNAPSHOT_VERSION,
                    sequence: seq,
                    record: rec,
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("delta must seed: {error}"));
    }

    let restored = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("restore must isolate foreign delta: {error}"));

    let counters = restored.provisional_evidence_counters().await;
    assert_eq!(counters.admitted(), 2);
    assert_eq!(counters.rejected_records(), 1);
    assert_eq!(counters.rejected_replay_markers(), 1);
    let page = restored
        .provisional_evidence_page(None, nonzero_usize(10))
        .await
        .unwrap_or_else(|error| panic!("page must project: {error}"));
    let mut restored_digests: Vec<_> = page.records().iter().map(|r| r.digest()).collect();
    restored_digests.sort();
    let mut expected_digests = vec![r1.digest(), r3.digest()];
    expected_digests.sort();
    assert_eq!(restored_digests, expected_digests);
}

#[tokio::test]
async fn explicit_flush_compacts_pending_deltas_into_snapshot() {
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let now_secs = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let r1 = make_evidence_record_for_provider(&provider, now_secs, 1);
    let r2 = make_evidence_record_for_provider(&provider, now_secs, 2);
    let r3 = make_evidence_record_for_provider(&provider, now_secs, 3);

    let evidence_storage = MapEvidenceStorage::default();
    let measure = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

    for record in [r1, r2] {
        assert_eq!(
            measure
                .admit_provisional_evidence(record)
                .await
                .unwrap_or_else(|error| panic!("admission must commit: {error}"))
                .admission(),
            EvidenceAdmission::Admitted
        );
    }
    assert!(!evidence_storage
        .snapshot_entries()
        .contains_key(EVIDENCE_SNAPSHOT_KEY));

    measure
        .flush()
        .await
        .unwrap_or_else(|error| panic!("explicit flush must compact pending deltas: {error}"));

    let flushed = evidence_storage.snapshot_entries();
    assert_eq!(flushed.len(), 1);
    match flushed.get(EVIDENCE_SNAPSHOT_KEY) {
        Some(EvidenceStorageEntry::Snapshot {
            compacted_through_sequence,
            snapshot,
        }) => {
            assert_eq!(*compacted_through_sequence, 2);
            assert_eq!(snapshot.records.len(), 2);
        }
        other => panic!("expected Snapshot checkpoint after flush, got {other:?}"),
    }

    drop(measure);
    let reopened = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        Box::new(evidence_storage.clone()),
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("reopen after flush must succeed: {error}"));
    assert_eq!(reopened.provisional_evidence_counters().await.admitted(), 2);
    reopened
        .admit_provisional_evidence(r3)
        .await
        .unwrap_or_else(|error| panic!("post-flush admission must succeed: {error}"));
    assert!(evidence_storage
        .snapshot_entries()
        .contains_key(&evidence_delta_key(3)));
}

#[tokio::test]
async fn legacy_bare_evidence_snapshot_file_is_retired_cleanly_on_startup() {
    let temp_dir =
        std::env::temp_dir().join(format!("rings-legacy-evidence-{}", uuid::Uuid::new_v4()));
    let temp_path = temp_dir
        .to_str()
        .unwrap_or_else(|| panic!("temp dir path must be UTF-8"))
        .to_string();
    let provider = DelegateeKey::new_with_seckey(&rings_core::ecc::SecretKey::random())
        .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let now_secs = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
    let legacy_record = make_evidence_record_for_provider(&provider, now_secs, 1);
    let fresh_record = make_evidence_record_for_provider(&provider, now_secs, 2);

    // Seed a legacy bare `EvidenceSnapshot<Did>` file at `EVIDENCE_SNAPSHOT_KEY` using the
    // production `RecordAuthority::Disposable` policy.
    {
        let legacy_file_store = FileStorage::new_with_cap_path_and_authority(
            1024 * 1024,
            temp_path.clone(),
            RecordAuthority::Disposable,
        )
        .await
        .unwrap_or_else(|error| panic!("legacy file storage must open: {error}"));
        let mut legacy_store = ProvisionalEvidenceStore::new(EvidenceLimits::default());
        legacy_store
            .admit(legacy_record)
            .unwrap_or_else(|error| panic!("legacy fixture must enter store: {error}"));
        KvStorageInterface::<EvidenceSnapshot<Did>>::put(
            &legacy_file_store,
            EVIDENCE_SNAPSHOT_KEY,
            &legacy_store.snapshot(),
        )
        .await
        .unwrap_or_else(|error| panic!("legacy snapshot must write: {error}"));
    }

    let upgraded_storage: EvidenceStorage = Box::new(
        FileStorage::new_with_cap_path_and_authority(
            1024 * 1024,
            temp_path.clone(),
            RecordAuthority::Disposable,
        )
        .await
        .unwrap_or_else(|error| panic!("upgraded file storage must reopen: {error}")),
    );
    let restored = PeriodicMeasure::new_with_clock_and_evidence(
        Box::new(MemStorage::new()),
        upgraded_storage,
        Some(collector),
        Arc::new(ManualMeasureClock::new(now_secs)),
    )
    .await
    .unwrap_or_else(|error| panic!("disposable store must retire legacy bare snapshot: {error}"));
    assert!(restored
        .provisional_evidence_page(None, nonzero_usize(4))
        .await
        .unwrap_or_else(|error| panic!("page must project: {error}"))
        .records()
        .is_empty());
    assert_eq!(
        restored
            .admit_provisional_evidence(fresh_record)
            .await
            .unwrap_or_else(|error| panic!(
                "fresh admission after legacy retirement must commit: {error}"
            ))
            .admission(),
        EvidenceAdmission::Admitted
    );
    restored.close().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
}
