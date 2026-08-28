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
use crate::MeasurementBatch;
use crate::MeasurementEvent;
use crate::MeasurementSnapshot;
use crate::ReliabilityClass;
use crate::ReliabilityEvidence;
use crate::ReliabilityPolicy;
use crate::ReliabilityWindow;
use crate::SnapshotRecord;
use crate::UnixTime;
use crate::MEASUREMENT_SNAPSHOT_VERSION;

/// Default hard bound on authenticated peer records retained by one ledger.
///
/// Adapters with a different memory budget can construct a ledger with
/// [`MeasurementLedger::with_max_records`]. The bound turns identity rotation
/// into a typed rejected transition instead of unbounded resident and snapshot
/// state.
pub const DEFAULT_MAX_RETAINED_PEERS: usize = 16_384;

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
            ReliabilityWindow::new(None, None, ReliabilityEvidence::new(0, 0, 0, 0, 0, 0)),
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

    fn transition_batch(
        self,
        batch: MeasurementBatch,
        at: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<Self, MeasureError> {
        let mut next = self;
        next.reliability
            .observe_batch(batch, at, reliability_policy)?;
        match batch.event() {
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
            self.reliability.window_seconds(),
            self.reliability.stored_evidence().is_unobserved(),
        ) {
            (None, _, false) => Err(MeasureError::SnapshotEvidenceWithoutEpoch),
            (Some(_), _, true) => Err(MeasureError::SnapshotEpochWithoutEvidence),
            (Some(_), None, false) => Err(MeasureError::SnapshotReliabilityWindowMissing),
            (None, Some(_), true) => Err(MeasureError::SnapshotReliabilityWindowWithoutEpoch),
            (Some(_), Some(0), false) => Err(MeasureError::SnapshotReliabilityWindowMissing),
            (Some(epoch_start), Some(window), false) if epoch_start.as_secs() % window != 0 => {
                Err(MeasureError::SnapshotReliabilityEpochMisaligned)
            }
            (Some(epoch_start), Some(_), false) if epoch_start > self.credit.last_seen() => {
                Err(MeasureError::SnapshotEpochAfterLastSeen)
            }
            _ => Ok(()),
        }
    }

    fn reconcile_clock(
        &mut self,
        now: UnixTime,
        policy: ReliabilityPolicy,
    ) -> Result<bool, MeasureError> {
        let credit_adjusted = self.credit.reconcile_clock(now);
        let reliability_adjusted = self.reliability.reconcile_clock(now, policy)?;
        Ok(credit_adjusted || reliability_adjusted)
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementLedger<P> {
    records: BTreeMap<P, PeerRecord>,
    max_records: usize,
}

impl<P> Default for MeasurementLedger<P>
where P: Ord
{
    fn default() -> Self {
        Self::new()
    }
}

impl<P> MeasurementLedger<P>
where P: Ord
{
    /// Construct an empty ledger.
    pub const fn new() -> Self {
        Self {
            records: BTreeMap::new(),
            max_records: DEFAULT_MAX_RETAINED_PEERS,
        }
    }

    /// Construct an empty ledger with an explicit non-zero retained-peer bound.
    pub const fn with_max_records(max_records: NonZeroUsize) -> Self {
        Self {
            records: BTreeMap::new(),
            max_records: max_records.get(),
        }
    }

    /// Maximum number of authenticated peer records this ledger can retain.
    pub const fn max_records(&self) -> usize {
        self.max_records
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
        self.apply_batch(
            peer,
            authentication,
            MeasurementBatch::single(event),
            at,
            reliability_policy,
        )
    }

    /// Apply a homogeneous batch as one atomic peer transition.
    ///
    /// On any counter, clock, or policy error, both byte credit and reliability
    /// evidence retain their complete prior state.
    pub fn apply_batch(
        &mut self,
        peer: P,
        authentication: Authentication,
        batch: MeasurementBatch,
        at: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<ApplyOutcome, MeasureError> {
        if matches!(authentication, Authentication::Unauthenticated) {
            return Ok(ApplyOutcome::IgnoredUnauthenticated);
        }

        if !self.records.contains_key(&peer) && self.records.len() >= self.max_records {
            return Err(MeasureError::RetainedPeerLimitExceeded {
                max: self.max_records,
            });
        }

        let current = self
            .records
            .get(&peer)
            .copied()
            .unwrap_or_else(|| PeerRecord::empty(at));
        let next = current.transition_batch(batch, at, reliability_policy)?;
        self.records.insert(peer, next);
        Ok(ApplyOutcome::Applied)
    }

    /// Read raw retained state for a peer without time projection.
    pub fn record(&self, peer: &P) -> Option<PeerRecord> {
        self.records.get(peer).copied()
    }

    /// Earliest exact retention boundary among retained records.
    ///
    /// Runtime adapters can schedule maintenance at this value without polling
    /// or allowing a coarse interval to expose already-expired credit.
    pub fn next_retention_boundary(&self, credit_policy: CreditPolicy) -> Option<UnixTime> {
        self.records
            .values()
            .map(|record| {
                UnixTime::from_secs(
                    record
                        .credit
                        .last_seen()
                        .as_secs()
                        .saturating_add(credit_policy.retention_seconds()),
                )
            })
            .min()
    }

    /// Validate and restore a ledger snapshot.
    pub fn from_snapshot(snapshot: MeasurementSnapshot<P>) -> Result<Self, MeasureError> {
        Self::from_snapshot_with_max_records(
            snapshot,
            NonZeroUsize::new(DEFAULT_MAX_RETAINED_PEERS).unwrap_or(NonZeroUsize::MIN),
        )
    }

    /// Validate and restore a ledger snapshot under an explicit non-zero retained-peer bound.
    pub fn from_snapshot_with_max_records(
        snapshot: MeasurementSnapshot<P>,
        max_records: NonZeroUsize,
    ) -> Result<Self, MeasureError> {
        if snapshot.schema_version != MEASUREMENT_SNAPSHOT_VERSION {
            return Err(MeasureError::UnsupportedSnapshotVersion {
                found: snapshot.schema_version,
            });
        }
        if snapshot.records.len() > max_records.get() {
            return Err(MeasureError::SnapshotPeerLimitExceeded {
                found: snapshot.records.len(),
                max: max_records.get(),
            });
        }

        let mut records = BTreeMap::new();
        for entry in snapshot.records {
            entry.record.validate_snapshot()?;
            if records.insert(entry.peer, entry.record).is_some() {
                return Err(MeasureError::DuplicatePeerInSnapshot);
            }
        }
        Ok(Self {
            records,
            max_records: max_records.get(),
        })
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
    ) -> Result<ClockReconciliation, MeasureError> {
        for record in self.records.values() {
            record.reliability.ensure_policy(reliability_policy)?;
        }
        let mut adjusted_records = 0;
        for record in self.records.values_mut() {
            adjusted_records += usize::from(record.reconcile_clock(now, reliability_policy)?);
        }
        Ok(ClockReconciliation { adjusted_records })
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
mod tests;
