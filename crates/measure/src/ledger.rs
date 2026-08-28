use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::ApplyOutcome;
use crate::Authentication;
use crate::CreditPolicy;
use crate::CreditRecord;
use crate::CreditScore;
use crate::MeasureError;
use crate::MeasurementEvent;
use crate::MeasurementSnapshot;
use crate::ReliabilityClass;
use crate::ReliabilityEvidence;
use crate::ReliabilityPolicy;
use crate::ReliabilityWindow;
use crate::SnapshotRecord;
use crate::UnixTime;
use crate::MEASUREMENT_SNAPSHOT_VERSION;

/// Pure credit and reliability state for one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    credit: CreditRecord,
    reliability: ReliabilityWindow,
}

impl PeerRecord {
    /// Construct explicit state, primarily for persistence migration.
    pub const fn new(credit: CreditRecord, reliability: ReliabilityWindow) -> Self {
        Self {
            credit,
            reliability,
        }
    }

    /// Construct an empty peer record at its first observation time.
    pub const fn empty(first_seen: UnixTime) -> Self {
        Self::new(
            CreditRecord::empty(first_seen),
            ReliabilityWindow::new(None, ReliabilityEvidence::new(0, 0, 0, 0, 0, 0)),
        )
    }

    /// Persistent byte-credit state.
    pub const fn credit(self) -> CreditRecord {
        self.credit
    }

    /// Recent reliability state.
    pub const fn reliability(self) -> ReliabilityWindow {
        self.reliability
    }

    fn transition(
        self,
        event: MeasurementEvent,
        at: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<Self, MeasureError> {
        let mut next = self;
        next.reliability.observe(event, at, reliability_policy)?;
        match event {
            MeasurementEvent::Sent { useful_bytes } => {
                next.credit.record_sent(useful_bytes, at)?;
            }
            MeasurementEvent::Received { useful_bytes } => {
                next.credit.record_received(useful_bytes, at)?;
            }
            MeasurementEvent::Connected
            | MeasurementEvent::Disconnected
            | MeasurementEvent::FailedToSend
            | MeasurementEvent::FailedToReceive => next.credit.touch(at)?,
        }
        Ok(next)
    }

    fn validate_snapshot(self) -> Result<(), MeasureError> {
        match (
            self.reliability.epoch_start(),
            self.reliability.stored_evidence().is_unobserved(),
        ) {
            (None, false) => Err(MeasureError::SnapshotEvidenceWithoutEpoch),
            (Some(_), true) => Err(MeasureError::SnapshotEpochWithoutEvidence),
            (Some(epoch_start), false) if epoch_start > self.credit.last_seen() => {
                Err(MeasureError::SnapshotEpochAfterLastSeen)
            }
            _ => Ok(()),
        }
    }
}

/// Projected measurement values for one peer at a caller-supplied time.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerMeasurement<P> {
    /// Stable peer key.
    pub peer: P,
    /// Persistent local byte totals and last-observation time.
    pub credit: CreditRecord,
    /// Opaque local credit multiplier.
    pub credit_score: CreditScore,
    /// Recent evidence live at the query time.
    pub reliability: ReliabilityEvidence,
    /// Advisory class derived from `reliability`.
    pub reliability_class: ReliabilityClass,
}

/// Pure local state relation keyed by a caller-defined stable peer identifier.
///
/// State transition:
/// `Ledger × Peer × Authentication × Event × Time -> Result<(Ledger, Outcome), Error>`.
/// The implementation mutates in place only after a complete peer transition
/// succeeds, so every error preserves the prior ledger.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MeasurementLedger<P> {
    records: BTreeMap<P, PeerRecord>,
}

impl<P> MeasurementLedger<P>
where P: Ord
{
    /// Construct an empty ledger.
    pub const fn new() -> Self {
        Self {
            records: BTreeMap::new(),
        }
    }

    /// Number of retained authenticated peer records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Return whether no authenticated peer record is retained.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Apply one logical event to one stable peer identity.
    pub fn apply(
        &mut self,
        peer: P,
        authentication: Authentication,
        event: MeasurementEvent,
        at: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<ApplyOutcome, MeasureError> {
        if matches!(authentication, Authentication::Unauthenticated) {
            return Ok(ApplyOutcome::IgnoredUnauthenticated);
        }

        let current = self
            .records
            .get(&peer)
            .copied()
            .unwrap_or_else(|| PeerRecord::empty(at));
        let next = current.transition(event, at, reliability_policy)?;
        self.records.insert(peer, next);
        Ok(ApplyOutcome::Applied)
    }

    /// Read raw retained state for a peer without time projection.
    pub fn record(&self, peer: &P) -> Option<PeerRecord> {
        self.records.get(peer).copied()
    }

    /// Validate and restore a ledger snapshot.
    pub fn from_snapshot(snapshot: MeasurementSnapshot<P>) -> Result<Self, MeasureError> {
        if snapshot.schema_version != MEASUREMENT_SNAPSHOT_VERSION {
            return Err(MeasureError::UnsupportedSnapshotVersion {
                found: snapshot.schema_version,
            });
        }

        let mut records = BTreeMap::new();
        for entry in snapshot.records {
            entry.record.validate_snapshot()?;
            if records.insert(entry.peer, entry.record).is_some() {
                return Err(MeasureError::DuplicatePeerInSnapshot);
            }
        }
        Ok(Self { records })
    }
}

impl<P> MeasurementLedger<P>
where P: Clone + Ord
{
    /// Project one retained record at `now` under explicit policies.
    pub fn measurement(
        &self,
        peer: &P,
        now: UnixTime,
        credit_policy: CreditPolicy,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<Option<PeerMeasurement<P>>, MeasureError> {
        self.records
            .get(peer)
            .copied()
            .map(|record| {
                record.credit.ensure_not_before_last_seen(now)?;
                let reliability = record.reliability.evidence_at(now, reliability_policy)?;
                Ok(PeerMeasurement {
                    peer: peer.clone(),
                    credit: record.credit,
                    credit_score: record.credit.score(credit_policy),
                    reliability,
                    reliability_class: reliability.classify_with_policy(reliability_policy),
                })
            })
            .transpose()
    }

    /// Project every retained peer in stable key order.
    pub fn measurements(
        &self,
        now: UnixTime,
        credit_policy: CreditPolicy,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<Vec<PeerMeasurement<P>>, MeasureError> {
        self.records
            .keys()
            .map(|peer| self.measurement(peer, now, credit_policy, reliability_policy))
            .filter_map(Result::transpose)
            .collect()
    }

    /// Remove records whose authenticated `last_seen` reached the retention boundary.
    ///
    /// The transition is atomic with respect to clock validation: a regression
    /// returns an error before any record is removed.
    pub fn prune(
        &mut self,
        now: UnixTime,
        credit_policy: CreditPolicy,
    ) -> Result<usize, MeasureError> {
        let mut expired = Vec::new();
        for (peer, record) in &self.records {
            if record.credit.is_expired(now, credit_policy)? {
                expired.push(peer.clone());
            }
        }
        let removed = expired.len();
        for peer in expired {
            self.records.remove(&peer);
        }
        Ok(removed)
    }

    /// Produce a deterministic versioned snapshot in peer-key order.
    pub fn snapshot(&self) -> MeasurementSnapshot<P> {
        MeasurementSnapshot {
            schema_version: MEASUREMENT_SNAPSHOT_VERSION,
            records: self
                .records
                .iter()
                .map(|(peer, record)| SnapshotRecord {
                    peer: peer.clone(),
                    record: *record,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
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
            Ok(ApplyOutcome::IgnoredUnauthenticated)
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
                        ReliabilityEvidence::new(0, 0, 9, 0, 0, 0),
                    ),
                ),
            }],
        };
        let mut ledger = MeasurementLedger::from_snapshot(snapshot)
            .unwrap_or_else(|error| invalid_fixture(error));
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
        let restored = MeasurementLedger::from_snapshot(decoded)
            .unwrap_or_else(|error| invalid_fixture(error));
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
                    ReliabilityWindow::new(None, ReliabilityEvidence::new(1, 0, 0, 0, 0, 0)),
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
                        ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
                    ),
                ),
            }],
        };
        assert_eq!(
            MeasurementLedger::from_snapshot(future_epoch),
            Err(MeasureError::SnapshotEpochAfterLastSeen)
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
        assert_eq!(ledger.prune(expiry, CreditPolicy::amule()), Ok(1));
        assert_eq!(ledger.prune(expiry, CreditPolicy::amule()), Ok(0));
        assert!(ledger.is_empty());
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
}
