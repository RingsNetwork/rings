use super::*;

fn limits(records: usize, bytes: usize, pair_records: usize, pair_bytes: usize) -> EvidenceLimits {
    EvidenceLimits {
        max_records: nonzero(records),
        max_bytes: nonzero(bytes),
        max_records_per_pair: nonzero(pair_records),
        max_bytes_per_pair: nonzero(pair_bytes),
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
fn snapshot_restore_skips_invalid_records_without_hiding_valid_records() -> Result<(), EvidenceError>
{
    let snapshot = EvidenceSnapshot {
        schema_version: EVIDENCE_SNAPSHOT_VERSION,
        records: vec![
            record(1, 2, 1, 1, 1, 1, 8),
            record(3, 4, 1, 2, 2, 2, 8),
            record(5, 6, 1, 3, 3, 3, 8),
        ],
        counters: EvidenceCounters::default(),
    };
    let (store, report) = ProvisionalEvidenceStore::from_snapshot_with_validator(
        snapshot,
        limits(8, 1024, 8, 1024),
        |record| record.digest() != EvidenceDigest::new([2; 32]),
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
    )?;

    assert_eq!(report, EvidenceLoadReport::default());
    assert_eq!(restored.counters(), counters);
    Ok(())
}
