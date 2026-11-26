use super::*;

fn limits(records: usize, bytes: usize, pair_records: usize, pair_bytes: usize) -> EvidenceLimits {
    EvidenceLimits {
        max_records: nonzero(records),
        max_bytes: nonzero(bytes),
        max_records_per_pair: nonzero(pair_records),
        max_bytes_per_pair: nonzero(pair_bytes),
        max_replay_markers: nonzero(records.saturating_mul(4)),
        max_replay_markers_per_beneficiary: nonzero(records.saturating_mul(4)),
    }
}

fn record(
    provider: u8,
    beneficiary: u8,
    slot: u64,
    nonce: u8,
    digest: u8,
    observed: u64,
    bytes: usize,
) -> ProvisionalEvidenceRecord<u8> {
    ProvisionalEvidenceRecord::new(
        EvidenceAccountPair::new(provider, beneficiary),
        EvidenceFreshnessKey::new(1, beneficiary, slot, [nonce; 32]),
        EvidenceDigest::new([digest; 32]),
        vec![digest; bytes],
        UnixTime::from_secs(observed),
        slot.saturating_sub(1),
    )
}

#[test]
fn duplicate_and_conflict_do_not_admit_second_record() -> Result<(), EvidenceError> {
    let mut store = ProvisionalEvidenceStore::new(limits(8, 1024, 8, 1024));
    let first = record(1, 2, 3, 4, 5, 6, 8);
    assert_eq!(
        store.admit(first.clone())?.admission(),
        EvidenceAdmission::Admitted
    );
    assert_eq!(
        store.admit(first)?.admission(),
        EvidenceAdmission::Duplicate
    );
    assert_eq!(
        store.admit(record(9, 2, 3, 4, 6, 7, 8))?.admission(),
        EvidenceAdmission::Conflict {
            retained: EvidenceDigest::new([5; 32])
        }
    );
    assert_eq!(store.len(), 1);
    assert_eq!(store.counters().duplicates(), 1);
    assert_eq!(store.counters().conflicts(), 1);
    Ok(())
}

#[test]
fn pair_and_global_bounds_evict_oldest_records_deterministically() -> Result<(), EvidenceError> {
    let mut store = ProvisionalEvidenceStore::new(limits(2, 32, 1, 16));
    store.admit(record(1, 2, 1, 1, 1, 1, 8))?;
    let pair_report = store.admit(record(1, 2, 1, 2, 2, 2, 8))?;
    assert_eq!(
        pair_report
            .evictions()
            .first()
            .map(EvidenceEviction::digest),
        Some(EvidenceDigest::new([1; 32]))
    );

    store.admit(record(3, 4, 1, 3, 3, 3, 8))?;
    let global_report = store.admit(record(5, 6, 1, 4, 4, 4, 8))?;
    assert_eq!(
        global_report
            .evictions()
            .first()
            .map(EvidenceEviction::digest),
        Some(EvidenceDigest::new([2; 32]))
    );
    assert_eq!(store.len(), 2);
    assert_eq!(store.resident_bytes(), 16);
    Ok(())
}

#[test]
fn admission_never_evicts_the_candidate_it_reports_as_admitted() -> Result<(), EvidenceError> {
    let mut store = ProvisionalEvidenceStore::new(limits(1, 8, 1, 8));
    let resident = record(1, 2, 1, 1, 2, 2, 8);
    let candidate = record(1, 2, 1, 2, 1, 2, 8);
    store.admit(resident.clone())?;

    let report = store.admit(candidate.clone())?;

    assert_eq!(report.admission(), EvidenceAdmission::Admitted);
    assert_eq!(report.evictions().len(), 1);
    assert_eq!(
        report.evictions().first().map(EvidenceEviction::digest),
        Some(resident.digest())
    );
    assert_eq!(store.len(), 1);
    assert_eq!(store.page(None, nonzero(1)).records(), &[candidate]);
    assert_eq!(store.counters().admitted(), 2);
    assert_eq!(store.counters().evicted_records(), 1);
    Ok(())
}

#[test]
fn admission_replaces_newer_resident_instead_of_discarding_older_candidate(
) -> Result<(), EvidenceError> {
    let mut store = ProvisionalEvidenceStore::new(limits(1, 8, 1, 8));
    let resident = record(1, 2, 1, 1, 1, 2, 8);
    let candidate = record(1, 2, 1, 2, 2, 1, 8);
    store.admit(resident.clone())?;

    let report = store.admit(candidate.clone())?;

    assert_eq!(report.admission(), EvidenceAdmission::Admitted);
    assert_eq!(report.evictions().len(), 1);
    assert_eq!(
        report.evictions().first().map(EvidenceEviction::digest),
        Some(resident.digest())
    );
    assert_eq!(store.page(None, nonzero(1)).records(), &[candidate]);
    Ok(())
}

#[test]
fn eviction_and_restart_retain_duplicate_and_conflict_protection() -> Result<(), EvidenceError> {
    let configured = limits(1, 8, 1, 8);
    let mut store = ProvisionalEvidenceStore::new(configured);
    let first = record(1, 2, 3, 1, 1, 3, 8);
    store.admit(first.clone())?;
    store.admit(record(1, 2, 3, 2, 2, 3, 8))?;
    assert_eq!(store.len(), 1);
    assert_eq!(store.replay_marker_len(), 2);

    let (mut restored, report) = ProvisionalEvidenceStore::from_snapshot_with_validator(
        store.snapshot(),
        configured,
        |_| true,
        |_| true,
    )?;
    assert_eq!(report, EvidenceLoadReport::default());
    assert_eq!(
        restored.admit(first)?.admission(),
        EvidenceAdmission::Duplicate
    );
    assert_eq!(
        restored.admit(record(9, 2, 3, 1, 3, 3, 8))?.admission(),
        EvidenceAdmission::Conflict {
            retained: EvidenceDigest::new([1; 32])
        }
    );
    Ok(())
}

#[test]
fn replay_capacity_and_clock_regression_fail_closed() -> Result<(), EvidenceError> {
    let mut configured = limits(4, 64, 4, 64);
    configured.max_replay_markers = nonzero(1);
    configured.max_replay_markers_per_beneficiary = nonzero(1);
    let mut store = ProvisionalEvidenceStore::new(configured);
    store.admit(record(1, 2, 3, 1, 1, 3, 8))?;
    assert_eq!(
        store.admit(record(1, 2, 3, 2, 2, 3, 8))?.admission(),
        EvidenceAdmission::ReplayCapacityExhausted
    );
    assert_eq!(store.counters().replay_capacity_rejections(), 1);
    assert_eq!(store.len(), 1);

    let mut regressed = ProvisionalEvidenceStore::new(limits(4, 64, 4, 64));
    regressed.admit(record(1, 2, 4, 1, 1, 4, 8))?;
    assert_eq!(
        regressed.admit(record(1, 2, 1, 2, 2, 1, 8)),
        Err(EvidenceError::FreshnessBeforeReplayFloor { epoch: 1, floor: 3 })
    );
    assert_eq!(regressed.len(), 1);
    Ok(())
}

#[test]
fn global_and_per_beneficiary_replay_bounds_are_independent() -> Result<(), EvidenceError> {
    let mut configured = limits(8, 1024, 8, 1024);
    configured.max_replay_markers = nonzero(3);
    configured.max_replay_markers_per_beneficiary = nonzero(1);
    let mut store = ProvisionalEvidenceStore::new(configured);

    assert_eq!(
        store.admit(record(1, 2, 3, 1, 1, 3, 8))?.admission(),
        EvidenceAdmission::Admitted
    );
    assert_eq!(
        store.admit(record(3, 2, 3, 2, 2, 3, 8))?.admission(),
        EvidenceAdmission::ReplayCapacityExhausted
    );
    assert_eq!(
        store.admit(record(3, 4, 3, 3, 3, 3, 8))?.admission(),
        EvidenceAdmission::Admitted
    );
    assert_eq!(
        store.admit(record(5, 6, 3, 4, 4, 3, 8))?.admission(),
        EvidenceAdmission::Admitted
    );
    assert_eq!(
        store.admit(record(7, 8, 3, 5, 5, 3, 8))?.admission(),
        EvidenceAdmission::ReplayCapacityExhausted
    );
    assert_eq!(store.replay_marker_len(), 3);
    assert_eq!(store.counters().replay_capacity_rejections(), 2);
    Ok(())
}

#[test]
fn malformed_record_sizes_fail_closed_without_state() {
    let mut store = ProvisionalEvidenceStore::new(limits(4, 16, 4, 8));

    assert_eq!(
        store.admit(record(1, 2, 3, 1, 1, 3, 0)),
        Err(EvidenceError::EmptyReceipt)
    );
    assert_eq!(
        store.admit(record(1, 2, 3, 2, 2, 3, 9)),
        Err(EvidenceError::ReceiptTooLarge { bytes: 9, limit: 8 })
    );
    assert!(store.is_empty());
    assert_eq!(store.replay_marker_len(), 0);
    assert_eq!(store.counters().rejected_records(), 2);
}

#[test]
fn snapshot_restore_skips_invalid_records_without_hiding_valid_records() -> Result<(), EvidenceError>
{
    let records = vec![
        record(1, 2, 1, 1, 1, 1, 8),
        record(3, 4, 1, 2, 2, 2, 8),
        record(5, 6, 1, 3, 3, 3, 8),
    ];
    let snapshot = EvidenceSnapshot {
        schema_version: EVIDENCE_SNAPSHOT_VERSION,
        replay_markers: records
            .iter()
            .map(EvidenceReplayMarker::from_record)
            .collect(),
        records,
        replay_floor: 0,
        counters: EvidenceCounters::default(),
    };
    let (store, report) = ProvisionalEvidenceStore::from_snapshot_with_validator(
        snapshot,
        limits(8, 1024, 8, 1024),
        |record| record.digest() != EvidenceDigest::new([2; 32]),
        |_| true,
    )?;
    assert_eq!(report.rejected_records(), 1);
    let page = store.page(None, nonzero(8));
    assert_eq!(page.records().len(), 2);
    assert_eq!(
        page.records()
            .first()
            .map(ProvisionalEvidenceRecord::digest),
        Some(EvidenceDigest::new([1; 32]))
    );
    assert_eq!(
        page.records().get(1).map(ProvisionalEvidenceRecord::digest),
        Some(EvidenceDigest::new([3; 32]))
    );
    Ok(())
}

#[test]
fn snapshot_restore_preserves_historical_counters_without_recounting_records(
) -> Result<(), EvidenceError> {
    let mut original = ProvisionalEvidenceStore::new(limits(8, 1024, 8, 1024));
    original.admit(record(1, 2, 1, 1, 1, 1, 8))?;
    original.admit(record(1, 2, 1, 1, 1, 1, 8))?;
    let counters = original.counters();

    let (restored, report) = ProvisionalEvidenceStore::from_snapshot_with_validator(
        original.snapshot(),
        limits(8, 1024, 8, 1024),
        |_| true,
        |_| true,
    )?;

    assert_eq!(report, EvidenceLoadReport::default());
    assert_eq!(restored.counters(), counters);
    Ok(())
}

#[test]
fn admit_rejects_mismatched_pair_and_freshness_beneficiary() {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;

    let hasher = RandomState::new();
    let nonce: [u8; 32] = std::array::from_fn(|index| hasher.hash_one(index) as u8);
    let mut store = ProvisionalEvidenceStore::new(limits(4, 64, 4, 64));
    let malformed = ProvisionalEvidenceRecord::new(
        EvidenceAccountPair::new(1, 2),
        EvidenceFreshnessKey::new(1, 9, 5, nonce),
        EvidenceDigest::new([7; 32]),
        vec![1, 2, 3, 4],
        UnixTime::from_secs(5),
        4,
    );
    assert_eq!(
        store.admit(malformed),
        Err(EvidenceError::BeneficiaryMismatch)
    );
    assert!(store.is_empty());
    assert_eq!(store.replay_floor(), 0);
    assert_eq!(store.counters().rejected_records(), 1);
}

/// Verify that incremental delta replay restores valid records in sequence order while
/// isolating corrupted or invalid records from valid neighbors.
#[test]
fn replay_persisted_delta_restores_valid_and_isolates_invalid_records() -> Result<(), EvidenceError>
{
    let mut store = ProvisionalEvidenceStore::new(limits(2, 64, 2, 64));
    let r1 = record(1, 2, 10, 1, 1, 10, 8);
    let r2 = record(1, 2, 10, 2, 2, 11, 8);
    let r3 = record(1, 2, 10, 3, 3, 12, 8);

    let rep1 = store.replay_persisted_delta(
        EvidenceAdmissionDelta {
            schema_version: EVIDENCE_SNAPSHOT_VERSION,
            sequence: 1,
            record: r1.clone(),
        },
        |_| true,
    )?;
    assert_eq!(rep1, EvidenceLoadReport::default());

    let rep_bad = store.replay_persisted_delta(
        EvidenceAdmissionDelta {
            schema_version: EVIDENCE_SNAPSHOT_VERSION,
            sequence: 2,
            record: r2,
        },
        |_| false,
    )?;
    assert_eq!(rep_bad.rejected_records(), 1);
    assert_eq!(rep_bad.rejected_replay_markers(), 1);

    let rep3 = store.replay_persisted_delta(
        EvidenceAdmissionDelta {
            schema_version: EVIDENCE_SNAPSHOT_VERSION,
            sequence: 3,
            record: r3.clone(),
        },
        |_| true,
    )?;
    assert_eq!(rep3, EvidenceLoadReport::default());

    assert_eq!(store.page(None, nonzero(4)).records(), &[r1, r3]);
    assert_eq!(store.counters().admitted(), 2);
    assert_eq!(store.counters().rejected_records(), 1);
    assert_eq!(store.counters().rejected_replay_markers(), 1);
    Ok(())
}
