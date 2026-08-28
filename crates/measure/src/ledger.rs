use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::ops::Bound::Excluded;
use std::ops::Bound::Unbounded;

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

    fn reconcile_clock(&mut self, now: UnixTime, policy: ReliabilityPolicy) -> bool {
        let credit_adjusted = self.credit.reconcile_clock(now);
        let reliability_adjusted = self.reliability.reconcile_clock(now, policy);
        credit_adjusted || reliability_adjusted
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

/// A peer whose retained state could not be projected or pruned independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMeasurementFailure<P> {
    peer: P,
    error: MeasureError,
}

impl<P> PeerMeasurementFailure<P> {
    fn new(peer: P, error: MeasureError) -> Self {
        Self { peer, error }
    }

    /// Stable peer key associated with the failure.
    pub const fn peer(&self) -> &P {
        &self.peer
    }

    /// Pure model error isolated to this peer.
    pub const fn error(&self) -> MeasureError {
        self.error
    }
}

/// Partial-success result from projecting every retained peer.
///
/// One malformed or future-dated record cannot hide healthy peers. Effectful
/// adapters should expose `measurements` and log or meter `failures`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasurementProjection<P> {
    measurements: Vec<PeerMeasurement<P>>,
    failures: Vec<PeerMeasurementFailure<P>>,
}

/// Bounded partial-success projection over a stable peer-key range.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasurementPage<P> {
    measurements: Vec<PeerMeasurement<P>>,
    failures: Vec<PeerMeasurementFailure<P>>,
    next_cursor: Option<P>,
}

impl<P> MeasurementPage<P> {
    /// Successfully projected peers among the bounded records scanned.
    pub fn measurements(&self) -> &[PeerMeasurement<P>] {
        &self.measurements
    }

    /// Per-peer failures among the bounded records scanned.
    pub fn failures(&self) -> &[PeerMeasurementFailure<P>] {
        &self.failures
    }

    /// Last scanned key when another retained record remains.
    pub const fn next_cursor(&self) -> Option<&P> {
        self.next_cursor.as_ref()
    }

    /// Consume the page into projections, failures, and its continuation key.
    pub fn into_parts(
        self,
    ) -> (
        Vec<PeerMeasurement<P>>,
        Vec<PeerMeasurementFailure<P>>,
        Option<P>,
    ) {
        (self.measurements, self.failures, self.next_cursor)
    }
}

impl<P> MeasurementProjection<P> {
    /// Successfully projected peers in stable key order.
    pub fn measurements(&self) -> &[PeerMeasurement<P>] {
        &self.measurements
    }

    /// Per-peer failures excluded from the successful projection.
    pub fn failures(&self) -> &[PeerMeasurementFailure<P>] {
        &self.failures
    }

    /// Consume the report into successful projections and isolated failures.
    pub fn into_parts(self) -> (Vec<PeerMeasurement<P>>, Vec<PeerMeasurementFailure<P>>) {
        (self.measurements, self.failures)
    }
}

/// Partial-success result from retention pruning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport<P> {
    removed: Vec<P>,
    failures: Vec<PeerMeasurementFailure<P>>,
}

impl<P> PruneReport<P> {
    /// Peer keys removed at the retention boundary.
    pub fn removed(&self) -> &[P] {
        &self.removed
    }

    /// Per-peer failures retained without blocking independent expiry.
    pub fn failures(&self) -> &[PeerMeasurementFailure<P>] {
        &self.failures
    }

    /// Number of records removed by this transition.
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// Summary of future timestamps clamped to an adapter-supplied wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockReconciliation {
    adjusted_records: usize,
}

impl ClockReconciliation {
    /// Number of peer records whose credit or reliability timestamp changed.
    pub const fn adjusted_records(self) -> usize {
        self.adjusted_records
    }

    /// Return whether any record required reconciliation.
    pub const fn is_adjusted(self) -> bool {
        self.adjusted_records > 0
    }
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
                project_record(peer.clone(), record, now, credit_policy, reliability_policy)
            })
            .transpose()
    }

    /// Project every retained peer in stable key order with per-peer failures.
    pub fn measurements(
        &self,
        now: UnixTime,
        credit_policy: CreditPolicy,
        reliability_policy: ReliabilityPolicy,
    ) -> MeasurementProjection<P> {
        let mut measurements = Vec::with_capacity(self.records.len());
        let mut failures = Vec::new();
        for (peer, record) in &self.records {
            match project_record(
                peer.clone(),
                *record,
                now,
                credit_policy,
                reliability_policy,
            ) {
                Ok(measurement) => measurements.push(measurement),
                Err(error) => {
                    failures.push(PeerMeasurementFailure::new(peer.clone(), error));
                }
            }
        }
        MeasurementProjection {
            measurements,
            failures,
        }
    }

    /// Project at most `limit` retained records after an exclusive peer cursor.
    ///
    /// Work and allocation are bounded by the requested page plus one key used
    /// to determine whether another page exists. Projection failures consume a
    /// page slot and are reported without preventing the cursor from advancing.
    pub fn measurements_page(
        &self,
        after: Option<&P>,
        limit: NonZeroUsize,
        now: UnixTime,
        credit_policy: CreditPolicy,
        reliability_policy: ReliabilityPolicy,
    ) -> MeasurementPage<P> {
        let mut records = match after {
            Some(peer) => self.records.range((Excluded(peer), Unbounded)),
            None => self.records.range(..),
        };
        let mut measurements = Vec::with_capacity(self.records.len().min(limit.get()));
        let mut failures = Vec::new();
        let mut last_scanned = None;
        for _ in 0..limit.get() {
            let Some((peer, record)) = records.next() else {
                break;
            };
            last_scanned = Some(peer.clone());
            match project_record(
                peer.clone(),
                *record,
                now,
                credit_policy,
                reliability_policy,
            ) {
                Ok(measurement) => measurements.push(measurement),
                Err(error) => failures.push(PeerMeasurementFailure::new(peer.clone(), error)),
            }
        }
        let next_cursor = if records.next().is_some() {
            last_scanned
        } else {
            None
        };
        MeasurementPage {
            measurements,
            failures,
            next_cursor,
        }
    }

    /// Remove records whose authenticated `last_seen` reached the retention boundary.
    ///
    /// A future-dated record is retained and reported without preventing other
    /// peers from expiring.
    pub fn prune(&mut self, now: UnixTime, credit_policy: CreditPolicy) -> PruneReport<P> {
        let mut expired = Vec::new();
        let mut failures = Vec::new();
        for (peer, record) in &self.records {
            match record.credit.is_expired(now, credit_policy) {
                Ok(true) => expired.push(peer.clone()),
                Ok(false) => {}
                Err(error) => {
                    failures.push(PeerMeasurementFailure::new(peer.clone(), error));
                }
            }
        }
        for peer in &expired {
            self.records.remove(peer);
        }
        PruneReport {
            removed: expired,
            failures,
        }
    }

    /// Clamp future timestamps after an external wall-clock regression.
    ///
    /// Credit totals and reliability evidence are preserved. `last_seen` is
    /// clamped to `now`, and a future reliability epoch is moved to the epoch
    /// containing `now`. This explicit recovery transition lets runtime
    /// adapters fail open while the default update/query relation continues to
    /// reject unacknowledged clock rollback.
    pub fn reconcile_clock(
        &mut self,
        now: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> ClockReconciliation {
        let adjusted_records = self.records.values_mut().fold(0, |count, record| {
            count + usize::from(record.reconcile_clock(now, reliability_policy))
        });
        ClockReconciliation { adjusted_records }
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

fn project_record<P>(
    peer: P,
    record: PeerRecord,
    now: UnixTime,
    credit_policy: CreditPolicy,
    reliability_policy: ReliabilityPolicy,
) -> Result<PeerMeasurement<P>, MeasureError> {
    record.credit.ensure_not_before_last_seen(now)?;
    let reliability = record.reliability.evidence_at(now, reliability_policy)?;
    Ok(PeerMeasurement {
        peer,
        credit: record.credit,
        credit_score: record.credit.score(credit_policy),
        reliability,
        reliability_class: reliability.classify_with_policy(reliability_policy),
    })
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
    fn bulk_projection_and_pruning_isolate_future_dated_peer() {
        let policy = reliability_policy();
        let valid = SnapshotRecord {
            peer: 1_u8,
            record: PeerRecord::new(
                CreditRecord::empty(UnixTime::from_secs(10)),
                ReliabilityWindow::new(
                    Some(UnixTime::EPOCH),
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
        let limit = NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN);

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
                        ReliabilityEvidence::new(1, 0, 0, 0, 0, 0),
                    ),
                ),
            }],
        })
        .unwrap_or_else(|error| invalid_fixture(error));
        let now = UnixTime::from_secs(20);

        let reconciliation = ledger.reconcile_clock(now, policy);

        assert_eq!(reconciliation.adjusted_records(), 1);
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
