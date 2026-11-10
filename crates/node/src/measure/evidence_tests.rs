use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::error::Error as CoreError;
use rings_core::message::MessageSigner;
use rings_core::message::ProvisionalEpoch;
use rings_core::message::ProvisionalServiceClaim;
use rings_core::message::ProvisionalServiceReceipt;
use rings_core::storage::file::FileStorage;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::MemStorage;

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
impl KvStorageInterface<EvidenceSnapshot<Did>> for FailingEvidenceStorage {
    async fn get(&self, _key: &str) -> rings_core::error::Result<Option<EvidenceSnapshot<Did>>> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &str,
        _value: &EvidenceSnapshot<Did>,
    ) -> rings_core::error::Result<()> {
        Err(CoreError::ServiceIOError(std::io::Error::other(
            "store unavailable",
        )))
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

fn provisional_evidence_fixture(
    tag: u8,
) -> (ProvisionalEvidenceRecord<Did>, EvidenceCollectorIdentity) {
    let provider = rings_core::delegation::DelegateeKey::new_with_seckey(
        &rings_core::ecc::SecretKey::random(),
    )
    .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let beneficiary = rings_core::delegation::DelegateeKey::new_with_seckey(
        &rings_core::ecc::SecretKey::random(),
    )
    .unwrap_or_else(|error| panic!("beneficiary delegation must build: {error}"));
    let observed_at_ms = rings_core::utils::get_epoch_ms();
    let observed_at_seconds = u64::try_from(observed_at_ms / 1_000)
        .unwrap_or_else(|_| panic!("current observation time must fit whole seconds"));
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
        .sign_provider(MessageSigner::new(&provider, 7))
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
    let record = ProvisionalEvidenceRecord::new(
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
    );
    let collector = EvidenceCollectorIdentity::new(claim.network_id, claim.provider_account);
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
            .put(EVIDENCE_SNAPSHOT_KEY, &snapshot_store.snapshot())
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
    impl KvStorageInterface<EvidenceSnapshot<Did>> for BlockingEvidenceStorage {
        async fn get(
            &self,
            _key: &str,
        ) -> rings_core::error::Result<Option<EvidenceSnapshot<Did>>> {
            Ok(None)
        }

        async fn put(
            &self,
            _key: &str,
            _value: &EvidenceSnapshot<Did>,
        ) -> rings_core::error::Result<()> {
            self.entered.notify_waiters();
            self.release.notified().await;
            Ok(())
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

    // Read queries complete immediately without waiting on persistence_lock and still see 0
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
        committed: Arc<std::sync::Mutex<Option<EvidenceSnapshot<Did>>>>,
        park_after_commit: Arc<AtomicBool>,
        fail_next_put: Arc<AtomicBool>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl KvStorageInterface<EvidenceSnapshot<Did>> for ControlledEvidenceStorage {
        async fn get(
            &self,
            _key: &str,
        ) -> rings_core::error::Result<Option<EvidenceSnapshot<Did>>> {
            Ok(self
                .committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone())
        }

        async fn put(
            &self,
            _key: &str,
            value: &EvidenceSnapshot<Did>,
        ) -> rings_core::error::Result<()> {
            if self.fail_next_put.swap(false, Ordering::SeqCst) {
                return Err(CoreError::ServiceIOError(std::io::Error::other(
                    "store unavailable",
                )));
            }
            *self
                .committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value.clone());
            if self.park_after_commit.swap(false, Ordering::SeqCst) {
                self.entered.notify_waiters();
                self.release.notified().await;
            }
            Ok(())
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

    let provider = rings_core::delegation::DelegateeKey::new_with_seckey(
        &rings_core::ecc::SecretKey::random(),
    )
    .unwrap_or_else(|error| panic!("provider delegation must build: {error}"));
    let collector = EvidenceCollectorIdentity::new(7, provider.delegator_did());
    let make_record = |tag: u8| {
        let beneficiary = rings_core::delegation::DelegateeKey::new_with_seckey(
            &rings_core::ecc::SecretKey::random(),
        )
        .unwrap_or_else(|error| panic!("beneficiary delegation must build: {error}"));
        let observed_at_seconds = 10_u64;
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
            .sign_provider(MessageSigner::new(&provider, 7))
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
            rings_measure::EvidenceAccountPair::new(
                claim.provider_account,
                claim.beneficiary_account,
            ),
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
    };
    let record_a = make_record(11);
    let record_b = make_record(21);

    let storage = ControlledEvidenceStorage {
        committed: Arc::new(std::sync::Mutex::new(None)),
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
    // Allow the cancelled task to drop its `persistence_lock` guard.
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
