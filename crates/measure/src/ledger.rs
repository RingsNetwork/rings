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

/// Default hard bound on attributable peer records retained by one ledger.
///
/// Adapters with a different memory budget can construct a ledger with
/// [`MeasurementLedger::with_max_records`]. At the bound, a successful event for
/// a new peer deterministically replaces the stalest record, keeping resident
/// and snapshot state bounded under identity rotation. Only authenticated peer
/// observations can establish new records; locally addressed failures for
/// unknown identities cannot consume this capacity.
#[allow(
    clippy::unwrap_used,
    reason = "the non-zero integer literal is validated during const evaluation"
)]
pub const DEFAULT_MAX_RETAINED_PEERS: NonZeroUsize = NonZeroUsize::new(16_384).unwrap();

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

    /// Construct an empty peer record at its first authenticated observation time.
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
        authentication: Authentication,
        batch: MeasurementBatch,
        at: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<Self, MeasureError> {
        let mut next = self;
        next.reliability
            .observe_batch(batch, at, reliability_policy)?;
        match (authentication.refreshes_peer_observation(), batch.event()) {
            (true, MeasurementEvent::Sent { useful_bytes }) => {
                next.credit.record_sent(useful_bytes, at)?;
            }
            (true, MeasurementEvent::Received { useful_bytes }) => {
                next.credit.record_received(useful_bytes, at)?;
            }
            (
                true,
                MeasurementEvent::Connected
                | MeasurementEvent::Disconnected
                | MeasurementEvent::FailedToSend
                | MeasurementEvent::FailedToReceive,
            ) => next.credit.touch(at)?,
            (false, _) => next.credit.ensure_not_before_last_seen(at)?,
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
            _ => Ok(()),
        }
    }

    fn reconcile_runtime(
        &mut self,
        now: UnixTime,
        policy: ReliabilityPolicy,
    ) -> RecordReconciliation {
        let reliability_reset = self.reliability.reset_for_policy_change(policy);
        let credit_adjusted = self.credit.reconcile_clock(now);
        let reliability_adjusted = self.reliability.reconcile_clock(now, policy);
        RecordReconciliation {
            clock_adjusted: credit_adjusted || reliability_adjusted,
            reliability_reset,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RecordReconciliation {
    clock_adjusted: bool,
    reliability_reset: bool,
}

/// Complete result of applying an observation to a bounded ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport<P> {
    outcome: ApplyOutcome,
    evicted_peer: Option<P>,
}

impl<P> ApplyReport<P> {
    /// Construct a report for an applied transition and its optional eviction.
    const fn applied(evicted_peer: Option<P>) -> Self {
        Self {
            outcome: ApplyOutcome::Applied,
            evicted_peer,
        }
    }

    /// Construct a report for an observation without attributable identity proof.
    const fn ignored_unattributable() -> Self {
        Self {
            outcome: ApplyOutcome::IgnoredUnattributable,
            evicted_peer: None,
        }
    }

    /// Construct a report for local evidence about an unknown peer.
    const fn ignored_unknown_peer() -> Self {
        Self {
            outcome: ApplyOutcome::IgnoredUnknownPeer,
            evicted_peer: None,
        }
    }

    /// Whether the requested observation changed retained state.
    pub const fn outcome(&self) -> ApplyOutcome {
        self.outcome
    }

    /// Peer deterministically removed to admit this transition, if any.
    pub const fn evicted_peer(&self) -> Option<&P> {
        self.evicted_peer.as_ref()
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

/// Summary of runtime-policy and wall-clock recovery transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LedgerReconciliation {
    clock_adjusted_records: usize,
    reliability_reset_records: usize,
}

impl LedgerReconciliation {
    /// Number of peer records whose credit or reliability timestamp was clamped.
    pub const fn clock_adjusted_records(self) -> usize {
        self.clock_adjusted_records
    }

    /// Number of peer records whose short-term evidence used an obsolete window.
    pub const fn reliability_reset_records(self) -> usize {
        self.reliability_reset_records
    }

    /// Return whether any record changed and therefore requires persistence.
    pub const fn is_adjusted(self) -> bool {
        self.clock_adjusted_records > 0 || self.reliability_reset_records > 0
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
    max_records: NonZeroUsize,
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
            max_records,
        }
    }

    /// Maximum number of attributable peer records this ledger can retain.
    pub const fn max_records(&self) -> usize {
        self.max_records.get()
    }

    /// Number of retained attributable peer records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Return whether no attributable peer record is retained.
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
    ) -> Result<ApplyReport<P>, MeasureError>
    where
        P: Clone,
    {
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
    /// evidence retain their complete prior state. Locally addressed evidence
    /// can update only a record previously established by authenticated peer
    /// observation and does not refresh its retention timestamp. A successful
    /// new-peer transition at capacity evicts the record with the earliest
    /// `last_seen`; peer-key order breaks timestamp ties. The report exposes the
    /// evicted key so effect adapters can make that loss observable.
    pub fn apply_batch(
        &mut self,
        peer: P,
        authentication: Authentication,
        batch: MeasurementBatch,
        at: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> Result<ApplyReport<P>, MeasureError>
    where
        P: Clone,
    {
        if !authentication.permits(batch.event()) {
            return Ok(ApplyReport::ignored_unattributable());
        }
        let is_new_peer = !self.records.contains_key(&peer);
        if is_new_peer && !authentication.establishes_peer() {
            return Ok(ApplyReport::ignored_unknown_peer());
        }
        let current = self
            .records
            .get(&peer)
            .copied()
            .unwrap_or_else(|| PeerRecord::empty(at));
        let next = current.transition_batch(authentication, batch, at, reliability_policy)?;
        let evicted_peer = if is_new_peer && self.records.len() >= self.max_records.get() {
            let stalest = self
                .records
                .iter()
                .min_by(|(left_peer, left_record), (right_peer, right_record)| {
                    left_record
                        .credit
                        .last_seen()
                        .cmp(&right_record.credit.last_seen())
                        .then_with(|| left_peer.cmp(right_peer))
                })
                .map(|(oldest_peer, _)| oldest_peer.clone());
            if let Some(stalest_peer) = &stalest {
                self.records.remove(stalest_peer);
            }
            stalest
        } else {
            None
        };
        self.records.insert(peer, next);
        Ok(ApplyReport::applied(evicted_peer))
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
        Self::from_snapshot_with_max_records(snapshot, DEFAULT_MAX_RETAINED_PEERS)
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
            max_records,
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

    /// Remove records whose attributable `last_seen` reached the retention boundary.
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

    /// Reconcile persisted advisory state with the active runtime policy and clock.
    ///
    /// Credit totals are always preserved. A future `last_seen` is clamped to
    /// `now`; a future reliability epoch moves to the epoch containing `now`.
    /// Reliability evidence recorded under a different aligned-window duration
    /// is reset because it cannot be projected into the new epoch relation.
    pub fn reconcile_runtime(
        &mut self,
        now: UnixTime,
        reliability_policy: ReliabilityPolicy,
    ) -> LedgerReconciliation {
        let mut reconciliation = LedgerReconciliation::default();
        for record in self.records.values_mut() {
            let adjusted = record.reconcile_runtime(now, reliability_policy);
            reconciliation.clock_adjusted_records += usize::from(adjusted.clock_adjusted);
            reconciliation.reliability_reset_records += usize::from(adjusted.reliability_reset);
        }
        reconciliation
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
