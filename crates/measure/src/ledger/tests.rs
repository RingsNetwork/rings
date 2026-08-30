use super::*;
use crate::PolicyError;
use crate::ReliabilityThresholds;

fn reliability_policy() -> ReliabilityPolicy {
    ReliabilityPolicy::new(60, 1, ReliabilityThresholds::new(3, 4, 5))
        .unwrap_or_else(|error| unreachable_policy(error))
}

fn unreachable_policy(error: PolicyError) -> ! {
    panic!("test policy must be valid: {error}")
}

#[test]
fn unauthenticated_events_cannot_create_or_change_credit() {
    let mut ledger = MeasurementLedger::<u8>::new();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Unauthenticated,
            MeasurementEvent::Received {
                useful_bytes: 2_000_000,
            },
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Ok(ApplyOutcome::IgnoredUnattributable)
    );
    assert!(ledger.is_empty());
}

#[test]
fn logical_transfer_updates_credit_and_reliability_once() {
    let mut ledger = MeasurementLedger::<u8>::new();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Received { useful_bytes: 42 },
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Ok(ApplyOutcome::Applied)
    );
    let record = ledger.record(&7).unwrap_or_else(|| missing_record());
    assert_eq!(record.credit().bytes_received_from_peer(), 42);
    assert_eq!(record.reliability().stored_evidence().received, 1);
}

#[test]
fn locally_addressed_failure_is_attributable_without_remote_authentication() {
    let mut ledger = MeasurementLedger::<u8>::new();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::LocallyAddressed,
            MeasurementEvent::FailedToSend,
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Ok(ApplyOutcome::Applied)
    );
    let record = ledger.record(&7).unwrap_or_else(|| missing_record());
    assert_eq!(record.reliability().stored_evidence().failed_to_send, 1);
}

#[test]
fn locally_addressed_proof_cannot_claim_successful_transfer_credit() {
    let mut ledger = MeasurementLedger::<u8>::new();
    for event in [
        MeasurementEvent::Sent { useful_bytes: 17 },
        MeasurementEvent::Received { useful_bytes: 19 },
    ] {
        assert_eq!(
            ledger.apply(
                7,
                Authentication::LocallyAddressed,
                event,
                UnixTime::from_secs(10),
                reliability_policy(),
            ),
            Ok(ApplyOutcome::IgnoredUnattributable)
        );
    }
    assert!(ledger.is_empty());
}

#[test]
fn retained_peer_bound_evicts_the_stalest_record_at_n_plus_one() {
    let limit = NonZeroUsize::MIN.saturating_add(1);
    let mut ledger = MeasurementLedger::<u8>::with_max_records(limit);
    let policy = reliability_policy();
    for (peer, observed_at) in [(1, 1), (2, 2)] {
        assert_eq!(
            ledger.apply(
                peer,
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(observed_at),
                policy,
            ),
            Ok(ApplyOutcome::Applied)
        );
    }
    assert_eq!(
        ledger.apply(
            1,
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 5 },
            UnixTime::from_secs(3),
            policy,
        ),
        Ok(ApplyOutcome::Applied),
        "updating a retained peer must refresh its eviction priority"
    );
    assert_eq!(
        ledger.apply(
            3,
            Authentication::Authenticated,
            MeasurementEvent::Connected,
            UnixTime::from_secs(4),
            policy,
        ),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger.snapshot().records.len(), 2);
    assert!(ledger.record(&1).is_some());
    assert!(ledger.record(&2).is_none());
    assert!(ledger.record(&3).is_some());
}

#[test]
fn snapshot_restore_enforces_configured_peer_bound() {
    let snapshot = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![
            SnapshotRecord {
                peer: 1_u8,
                record: PeerRecord::empty(UnixTime::EPOCH),
            },
            SnapshotRecord {
                peer: 2_u8,
                record: PeerRecord::empty(UnixTime::EPOCH),
            },
        ],
    };
    let limit = NonZeroUsize::MIN;
    assert_eq!(
        MeasurementLedger::from_snapshot_with_max_records(snapshot, limit),
        Err(MeasureError::SnapshotPeerLimitExceeded { found: 2, max: 1 })
    );
}

fn missing_record() -> ! {
    panic!("authenticated event must create a record")
}

#[test]
fn failed_transition_preserves_complete_prior_record() {
    let snapshot = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 7_u8,
            record: PeerRecord::new(
                CreditRecord::new(u64::MAX, 0, UnixTime::from_secs(10)),
                ReliabilityWindow::new(
                    Some(UnixTime::EPOCH),
                    Some(60),
                    ReliabilityEvidence::new(0, 0, 9, 0, 0, 0),
                ),
            ),
        }],
    };
    let mut ledger =
        MeasurementLedger::from_snapshot(snapshot).unwrap_or_else(|error| invalid_fixture(error));
    let before = ledger.clone();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 1 },
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Err(MeasureError::CounterOverflow {
            metric: crate::Metric::BytesSent,
        })
    );
    assert_eq!(ledger, before);
}

#[test]
fn homogeneous_batch_updates_count_and_aggregate_bytes_atomically() {
    let mut ledger = MeasurementLedger::<u8>::new();
    let occurrences = std::num::NonZeroU64::MIN.saturating_add(2);
    assert_eq!(
        ledger.apply_batch(
            7,
            Authentication::Authenticated,
            MeasurementBatch::new(MeasurementEvent::Sent { useful_bytes: 21 }, occurrences,),
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Ok(ApplyOutcome::Applied)
    );
    let record = ledger.record(&7).unwrap_or_else(|| missing_record());
    assert_eq!(record.credit().bytes_sent_to_peer(), 21);
    assert_eq!(record.reliability().stored_evidence().sent, 3);
}

#[test]
fn batch_byte_overflow_preserves_reliability_and_credit() {
    let snapshot = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 7_u8,
            record: PeerRecord::new(
                CreditRecord::new(u64::MAX - 5, 0, UnixTime::from_secs(10)),
                ReliabilityWindow::new(
                    Some(UnixTime::EPOCH),
                    Some(60),
                    ReliabilityEvidence::new(0, 0, 9, 0, 0, 0),
                ),
            ),
        }],
    };
    let mut ledger =
        MeasurementLedger::from_snapshot(snapshot).unwrap_or_else(|error| invalid_fixture(error));
    let before = ledger.clone();
    let occurrences = std::num::NonZeroU64::MIN.saturating_add(1);
    assert_eq!(
        ledger.apply_batch(
            7,
            Authentication::Authenticated,
            MeasurementBatch::new(MeasurementEvent::Sent { useful_bytes: 8 }, occurrences,),
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Err(MeasureError::CounterOverflow {
            metric: crate::Metric::BytesSent,
        })
    );
    assert_eq!(ledger, before);
}

#[test]
fn batch_reliability_overflow_preserves_complete_prior_record() {
    let snapshot = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 7_u8,
            record: PeerRecord::new(
                CreditRecord::new(5, 0, UnixTime::from_secs(10)),
                ReliabilityWindow::new(
                    Some(UnixTime::EPOCH),
                    Some(60),
                    ReliabilityEvidence::new(0, 0, u64::MAX - 1, 0, 0, 0),
                ),
            ),
        }],
    };
    let mut ledger =
        MeasurementLedger::from_snapshot(snapshot).unwrap_or_else(|error| invalid_fixture(error));
    let before = ledger.clone();
    let occurrences = std::num::NonZeroU64::MIN.saturating_add(1);
    assert_eq!(
        ledger.apply_batch(
            7,
            Authentication::Authenticated,
            MeasurementBatch::new(MeasurementEvent::Sent { useful_bytes: 1 }, occurrences,),
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Err(MeasureError::CounterOverflow {
            metric: crate::Metric::Sent,
        })
    );
    assert_eq!(ledger, before);
}

fn invalid_fixture(error: MeasureError) -> ! {
    panic!("test snapshot must be valid: {error}")
}

#[test]
fn snapshot_json_round_trip_preserves_projected_measurements() {
    let mut ledger = MeasurementLedger::<u8>::new();
    let policy = reliability_policy();
    for event in [
        MeasurementEvent::Connected,
        MeasurementEvent::Sent { useful_bytes: 13 },
        MeasurementEvent::Received {
            useful_bytes: 2_000_000,
        },
    ] {
        assert_eq!(
            ledger.apply(
                7,
                Authentication::Authenticated,
                event,
                UnixTime::from_secs(10),
                policy,
            ),
            Ok(ApplyOutcome::Applied)
        );
    }
    let encoded = serde_json::to_string(&ledger.snapshot())
        .unwrap_or_else(|error| panic!("snapshot must serialize: {error}"));
    let decoded: MeasurementSnapshot<u8> = serde_json::from_str(&encoded)
        .unwrap_or_else(|error| panic!("snapshot must deserialize: {error}"));
    let restored =
        MeasurementLedger::from_snapshot(decoded).unwrap_or_else(|error| invalid_fixture(error));
    assert_eq!(restored, ledger);
    assert_eq!(
        restored.measurements(UnixTime::from_secs(10), CreditPolicy::amule(), policy),
        ledger.measurements(UnixTime::from_secs(10), CreditPolicy::amule(), policy)
    );
}

#[test]
fn snapshot_rejects_unknown_version_and_duplicate_peer() {
    let unsupported = MeasurementSnapshot::<u8> {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION + 1,
        records: Vec::new(),
    };
    assert!(matches!(
        MeasurementLedger::from_snapshot(unsupported),
        Err(MeasureError::UnsupportedSnapshotVersion { .. })
    ));

    let record = SnapshotRecord {
        peer: 7_u8,
        record: PeerRecord::empty(UnixTime::EPOCH),
    };
    let duplicate = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![record.clone(), record],
    };
    assert_eq!(
        MeasurementLedger::from_snapshot(duplicate),
        Err(MeasureError::DuplicatePeerInSnapshot)
    );
}

#[test]
fn snapshot_rejects_inconsistent_reliability_state() {
    let evidence_without_epoch = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 1_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(10)),
                ReliabilityWindow::new(None, None, ReliabilityEvidence::new(1, 0, 0, 0, 0, 0)),
            ),
        }],
    };
    assert_eq!(
        MeasurementLedger::from_snapshot(evidence_without_epoch),
        Err(MeasureError::SnapshotEvidenceWithoutEpoch)
    );

    let future_epoch = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 1_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(10)),
                ReliabilityWindow::new(
                    Some(UnixTime::from_secs(60)),
                    Some(60),
                    ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
                ),
            ),
        }],
    };
    assert_eq!(
        MeasurementLedger::from_snapshot(future_epoch),
        Err(MeasureError::SnapshotEpochAfterLastSeen)
    );

    let missing_window = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 1_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(10)),
                ReliabilityWindow::new(
                    Some(UnixTime::EPOCH),
                    None,
                    ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
                ),
            ),
        }],
    };
    assert_eq!(
        MeasurementLedger::from_snapshot(missing_window),
        Err(MeasureError::SnapshotReliabilityWindowMissing)
    );

    let window_without_epoch = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 1_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(10)),
                ReliabilityWindow::new(None, Some(60), ReliabilityEvidence::default()),
            ),
        }],
    };
    assert_eq!(
        MeasurementLedger::from_snapshot(window_without_epoch),
        Err(MeasureError::SnapshotReliabilityWindowWithoutEpoch)
    );

    let misaligned_epoch = MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 1_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(100)),
                ReliabilityWindow::new(
                    Some(UnixTime::from_secs(61)),
                    Some(60),
                    ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
                ),
            ),
        }],
    };
    assert_eq!(
        MeasurementLedger::from_snapshot(misaligned_epoch),
        Err(MeasureError::SnapshotReliabilityEpochMisaligned)
    );
}

#[test]
fn changing_reliability_window_rejects_without_mutation() {
    let short = reliability_policy();
    let long = ReliabilityPolicy::new(3_600, 1, short.thresholds())
        .unwrap_or_else(|error| unreachable_policy(error));
    let mut ledger = MeasurementLedger::<u8>::new();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Connected,
            UnixTime::from_secs(120),
            short,
        ),
        Ok(ApplyOutcome::Applied)
    );
    let before = ledger.clone();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Disconnected,
            UnixTime::from_secs(120),
            long,
        ),
        Err(MeasureError::ReliabilityWindowMismatch {
            stored_seconds: 60,
            supplied_seconds: 3_600,
        })
    );
    assert_eq!(ledger, before);
}

#[test]
fn runtime_reconciliation_resets_obsolete_window_and_preserves_credit() {
    let short = reliability_policy();
    let long = ReliabilityPolicy::new(3_600, 1, short.thresholds())
        .unwrap_or_else(|error| unreachable_policy(error));
    let mut ledger = MeasurementLedger::<u8>::new();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Sent { useful_bytes: 41 },
            UnixTime::from_secs(120),
            short,
        ),
        Ok(ApplyOutcome::Applied)
    );

    let reconciliation = ledger.reconcile_runtime(UnixTime::from_secs(120), long);

    assert_eq!(reconciliation.clock_adjusted_records(), 0);
    assert_eq!(reconciliation.reliability_reset_records(), 1);
    let record = ledger.record(&7).unwrap_or_else(|| missing_record());
    assert_eq!(record.credit().bytes_sent_to_peer(), 41);
    assert!(record.reliability().stored_evidence().is_unobserved());
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Connected,
            UnixTime::from_secs(120),
            long,
        ),
        Ok(ApplyOutcome::Applied)
    );
}

#[test]
fn query_clock_rollback_does_not_project_future_state() {
    let mut ledger = MeasurementLedger::<u8>::new();
    let policy = reliability_policy();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Connected,
            UnixTime::from_secs(50),
            policy,
        ),
        Ok(ApplyOutcome::Applied)
    );
    assert!(matches!(
        ledger.measurement(&7, UnixTime::from_secs(49), CreditPolicy::amule(), policy,),
        Err(MeasureError::ClockRegression { .. })
    ));
}

#[test]
fn bulk_projection_and_pruning_isolate_future_dated_peer() {
    let policy = reliability_policy();
    let valid = SnapshotRecord {
        peer: 1_u8,
        record: PeerRecord::new(
            CreditRecord::empty(UnixTime::from_secs(10)),
            ReliabilityWindow::new(
                Some(UnixTime::EPOCH),
                Some(60),
                ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
            ),
        ),
    };
    let future = SnapshotRecord {
        peer: 2_u8,
        record: PeerRecord::new(
            CreditRecord::empty(UnixTime::from_secs(100)),
            ReliabilityWindow::new(
                Some(UnixTime::from_secs(60)),
                Some(60),
                ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
            ),
        ),
    };
    let mut ledger = MeasurementLedger::from_snapshot(MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![valid, future],
    })
    .unwrap_or_else(|error| invalid_fixture(error));
    let now = UnixTime::from_secs(20);

    let projection = ledger.measurements(now, CreditPolicy::amule(), policy);
    assert_eq!(
        projection
            .measurements()
            .first()
            .map(|measurement| measurement.peer),
        Some(1)
    );
    assert_eq!(projection.measurements().len(), 1);
    assert_eq!(projection.failures().len(), 1);
    assert_eq!(
        projection
            .failures()
            .first()
            .map(PeerMeasurementFailure::peer),
        Some(&2)
    );

    let retention = CreditPolicy::new(0, 10).unwrap_or_else(|error| unreachable_policy(error));
    let pruning = ledger.prune(now, retention);
    assert_eq!(pruning.removed(), &[1]);
    assert_eq!(pruning.failures().len(), 1);
    assert!(ledger.record(&2).is_some());
}

#[test]
fn bounded_page_projects_only_scanned_records_and_advances_past_failures() {
    let policy = reliability_policy();
    let record_at = |last_seen| {
        PeerRecord::new(
            CreditRecord::empty(UnixTime::from_secs(last_seen)),
            ReliabilityWindow::new(
                Some(UnixTime::EPOCH),
                Some(60),
                ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
            ),
        )
    };
    let ledger = MeasurementLedger::from_snapshot(MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![
            SnapshotRecord {
                peer: 1_u8,
                record: record_at(10),
            },
            SnapshotRecord {
                peer: 2_u8,
                record: record_at(10),
            },
            SnapshotRecord {
                peer: 3_u8,
                record: record_at(100),
            },
            SnapshotRecord {
                peer: 4_u8,
                record: record_at(10),
            },
        ],
    })
    .unwrap_or_else(|error| invalid_fixture(error));
    let limit = NonZeroUsize::MIN.saturating_add(1);

    let first = ledger.measurements_page(
        None,
        limit,
        UnixTime::from_secs(20),
        CreditPolicy::amule(),
        policy,
    );
    assert_eq!(
        first
            .measurements()
            .iter()
            .map(|measurement| measurement.peer)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(first.failures().is_empty());
    assert_eq!(first.next_cursor(), Some(&2));

    let second = ledger.measurements_page(
        first.next_cursor(),
        limit,
        UnixTime::from_secs(20),
        CreditPolicy::amule(),
        policy,
    );
    assert_eq!(
        second
            .measurements()
            .first()
            .map(|measurement| measurement.peer),
        Some(4)
    );
    assert_eq!(second.measurements().len(), 1);
    assert_eq!(
        second.failures().first().map(PeerMeasurementFailure::peer),
        Some(&3)
    );
    assert!(second.next_cursor().is_none());
}

#[test]
fn explicit_clock_reconciliation_clamps_future_timestamps() {
    let policy = reliability_policy();
    let mut ledger = MeasurementLedger::from_snapshot(MeasurementSnapshot {
        schema_version: MEASUREMENT_SNAPSHOT_VERSION,
        records: vec![SnapshotRecord {
            peer: 2_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(100)),
                ReliabilityWindow::new(
                    Some(UnixTime::from_secs(60)),
                    Some(60),
                    ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
                ),
            ),
        }],
    })
    .unwrap_or_else(|error| invalid_fixture(error));
    let now = UnixTime::from_secs(20);

    let reconciliation = ledger.reconcile_runtime(now, policy);

    assert_eq!(reconciliation.clock_adjusted_records(), 1);
    assert_eq!(reconciliation.reliability_reset_records(), 0);
    let record = ledger.record(&2).unwrap_or_else(|| missing_record());
    assert_eq!(record.credit().last_seen(), now);
    assert_eq!(record.reliability().epoch_start(), Some(UnixTime::EPOCH));
    assert!(ledger
        .measurement(&2, now, CreditPolicy::amule(), policy)
        .is_ok());
}

#[test]
fn pruning_is_idempotent_at_retention_boundary() {
    let mut ledger = MeasurementLedger::<u8>::new();
    assert_eq!(
        ledger.apply(
            7,
            Authentication::Authenticated,
            MeasurementEvent::Connected,
            UnixTime::from_secs(10),
            reliability_policy(),
        ),
        Ok(ApplyOutcome::Applied)
    );
    let expiry = UnixTime::from_secs(10 + CreditPolicy::amule().retention_seconds());
    assert_eq!(
        ledger.prune(expiry, CreditPolicy::amule()).removed_count(),
        1
    );
    assert_eq!(
        ledger.prune(expiry, CreditPolicy::amule()).removed_count(),
        0
    );
    assert!(ledger.is_empty());
}

#[test]
fn next_retention_boundary_tracks_the_earliest_peer_exactly() {
    let mut ledger = MeasurementLedger::<u8>::new();
    let policy = reliability_policy();
    for (peer, observed_at) in [(1, 20), (2, 10), (3, 30)] {
        assert_eq!(
            ledger.apply(
                peer,
                Authentication::Authenticated,
                MeasurementEvent::Connected,
                UnixTime::from_secs(observed_at),
                policy,
            ),
            Ok(ApplyOutcome::Applied)
        );
    }
    assert_eq!(
        ledger.next_retention_boundary(CreditPolicy::amule()),
        Some(UnixTime::from_secs(
            10 + CreditPolicy::amule().retention_seconds()
        ))
    );
}

#[test]
fn every_two_event_sequence_matches_counter_homomorphism() {
    let events = [
        MeasurementEvent::Connected,
        MeasurementEvent::Disconnected,
        MeasurementEvent::Sent { useful_bytes: 3 },
        MeasurementEvent::FailedToSend,
        MeasurementEvent::Received { useful_bytes: 5 },
        MeasurementEvent::FailedToReceive,
    ];
    for first in events {
        for second in events {
            let mut ledger = MeasurementLedger::<u8>::new();
            for event in [first, second] {
                assert_eq!(
                    ledger.apply(
                        1,
                        Authentication::Authenticated,
                        event,
                        UnixTime::from_secs(1),
                        reliability_policy(),
                    ),
                    Ok(ApplyOutcome::Applied)
                );
            }
            let record = ledger.record(&1).unwrap_or_else(|| missing_record());
            let evidence = record.reliability().stored_evidence();
            assert_eq!(
                evidence.connected
                    + evidence.disconnected
                    + evidence.sent
                    + evidence.failed_to_send
                    + evidence.received
                    + evidence.failed_to_receive,
                2
            );
        }
    }
}
