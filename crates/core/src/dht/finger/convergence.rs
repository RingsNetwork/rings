//! Pure state machine for range-aware Chord finger convergence.
//!
//! This module coordinates four independent concerns and deliberately owns no
//! network, lock, clock, or random-number effects:
//!
//! - [`proof`] derives which consecutive slots one Chord lookup proves;
//! - [`evidence`] versions the knowledge attached to each local finger hint;
//! - [`attempt`] gives one lookup exactly one ownership phase;
//! - [`retry`] records the deterministic lower bound for the next emission.
//!
//! The runtime supplies `now_ms` and fresh UUIDs, interprets returned actions,
//! and adds jitter. Keeping that boundary outside this module makes every
//! protocol transition deterministic and model-testable.
//!
//! # State relation
//!
//! 1. A slot is verified only by a proof issued no earlier than its last hint
//!    change.
//! 2. At most one attempt exists: idle, awaiting a report, or awaiting
//!    admission. Illegal combinations such as two simultaneous owners are not
//!    representable.
//! 3. Only the exact `(slot, UUID)` token may consume an attempt. Reordered
//!    reports are stale observations, not retry failures.
//! 4. A failure increases the retry floor. Only committed evidence (or an
//!    equivalent locally confirmed range) resets it; merely starting a
//!    handshake is not progress.

/// Attempt ownership for in-flight reports and retained admission proofs.
mod attempt;
/// Evidence epochs and per-slot verification bits for finger hints.
mod evidence;
/// Chord range proofs and request/rejection wire types.
mod proof;
/// Deterministic retry floor for failed finger lookups.
mod retry;
/// Scheduler-facing convergence status projection.
mod status;

use serde::Deserialize;
use serde::Serialize;

use self::attempt::FingerAttempt;
use self::attempt::FingerAttemptStatus;
use self::attempt::FingerProofSource;
use self::evidence::EvidenceInvalidation;
use self::evidence::FingerEvidence;
pub(crate) use self::proof::finger_proof_end;
pub(crate) use self::proof::FingerApplyOutcome;
pub(crate) use self::proof::FingerDeferOutcome;
pub use self::proof::FingerFixRequest;
use self::proof::FingerRangeProof;
pub(crate) use self::proof::FingerReportRejection;
pub(crate) use self::proof::FingerRetireOutcome;
pub(crate) use self::retry::finger_lookup_backoff_ms;
use self::retry::FingerRetryState;
#[cfg(test)]
pub(crate) use self::retry::FINGER_LOOKUP_MIN_INTERVAL_MS;
pub(crate) use self::status::FingerConvergencePhase;
pub(crate) use self::status::FingerConvergenceStatus;
use crate::dht::Did;

/// Time after which an unanswered lookup ceases to own the convergence slot.
const FINGER_LOOKUP_TIMEOUT_MS: u64 = 10_000;

/// Maximum time a timely proof may wait for transport admission.
///
/// This matches the pending WebRTC generation lease and also bounds the window
/// before a connection generation has been allocated. Expiry counts as a
/// failed attempt and therefore enters exponential backoff.
pub(crate) const FINGER_ADMISSION_TIMEOUT_MS: u64 = 180_000;

/// Serializable protocol state for one node's local finger convergence.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct FingerConvergenceState {
    /// Per-slot verification bits plus the hint-change epoch they belong to.
    evidence: FingerEvidence,
    /// The one lookup or admission proof currently owned by this state.
    attempt: FingerAttempt,
    /// Deterministic retry floor applied before the scheduler adds jitter.
    retry: FingerRetryState,
}

/// Test-only observation used by the retry and interleaving models.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceProjection {
    /// Snapshot of each slot's verified bit in table order.
    pub(crate) verified: Vec<bool>,
    /// Request waiting for a successor report.
    pub(crate) in_flight: Option<FingerFixRequest>,
    /// Request whose proof is held while transport admission finishes.
    pub(crate) deferred: Option<FingerFixRequest>,
    /// Admission lease deadline for `deferred`, when present.
    pub(crate) deferred_expires_at_ms: Option<u64>,
    /// Report deadline for `in_flight`, when present.
    pub(crate) expires_at_ms: Option<u64>,
    /// Monotonic timestamp of the last emitted automatic lookup.
    pub(crate) last_issued_at_ms: Option<u64>,
    /// Number of consecutive current-attempt failures since last progress.
    pub(crate) failure_streak: u8,
    /// Earliest deterministic retry timestamp after failures.
    pub(crate) retry_not_before_ms: Option<u64>,
}

impl FingerConvergenceState {
    /// Create fully unverified convergence state for a table width.
    pub(crate) fn new(slot_count: usize) -> Self {
        Self {
            evidence: FingerEvidence::new(slot_count),
            attempt: FingerAttempt::Idle,
            retry: FingerRetryState::default(),
        }
    }

    /// Clamp restored state to the current table width.
    ///
    /// Retry timestamps are left intact because they are clock-relative
    /// scheduler policy; evidence and owned proofs are the width-dependent
    /// parts that can otherwise point outside the resized table.
    pub(crate) fn normalized(mut self, slot_count: usize) -> Self {
        self.evidence.normalize(slot_count);
        self.attempt.normalize(slot_count);
        self
    }

    /// Convert internal evidence and attempt ownership into scheduler state.
    ///
    /// `first_routable_slot` excludes the local successor range already proved
    /// by stabilization, so a node does not keep issuing Chord lookups for
    /// slots whose target is known to resolve locally.
    pub(crate) fn status_after(
        &self,
        first_routable_slot: usize,
        now_ms: u64,
    ) -> FingerConvergenceStatus {
        let failure_streak = self.retry.failure_streak();
        match self.attempt.status(now_ms) {
            FingerAttemptStatus::Idle => FingerConvergenceStatus::new(
                self.evidence.any_unverified_from(first_routable_slot),
                failure_streak,
            ),
            FingerAttemptStatus::AwaitingReport { remaining_ms } => {
                FingerConvergenceStatus::awaiting_report(remaining_ms, failure_streak)
            }
            FingerAttemptStatus::AwaitingAdmission { remaining_ms } => {
                FingerConvergenceStatus::awaiting_admission(remaining_ms, failure_streak)
            }
        }
    }

    /// Invalidate only evidence whose inferred finger hint changed.
    ///
    /// A change at the active request's lower slot destroys the premise of its
    /// range proof, so that attempt is retired. Changes elsewhere are recorded
    /// by epoch and will be skipped if an older range result later arrives.
    pub(crate) fn invalidate_hint_changes(
        &mut self,
        before: &[Option<Did>],
        after: &[Option<Did>],
    ) {
        // The lower slot is the premise of the proof range; if that exact hint
        // changed, an otherwise current token must stop owning the lookup.
        let active_hint_changed = self.attempt.request().is_some_and(|request| {
            FingerEvidence::hint_changed_at(before, after, request.slot_index())
        });
        let invalidation = self.evidence.invalidate_hint_changes(before, after);
        if active_hint_changed || matches!(invalidation, EvidenceInvalidation::EpochExhausted) {
            self.attempt.clear();
        }
    }

    /// Forget every proof after a membership discontinuity.
    pub(crate) fn invalidate_all_evidence(&mut self) {
        if self.attempt.is_idle() && self.evidence.all_unverified() {
            return;
        }
        self.evidence.invalidate_all();
        self.attempt.clear();
    }

    /// Reopen one consecutive hint range after a completed convergence pass.
    pub(crate) fn begin_revalidation(&mut self, fingers: &[Option<Did>], cursor: usize) {
        if self.attempt.is_idle() && self.evidence.all_verified() {
            self.evidence.reopen_next_range(fingers, cursor);
        }
    }

    /// Reserve the next lookup if ownership, evidence, and pacing allow it.
    ///
    /// Timeout processing happens before reservation. This call intentionally
    /// returns `None` on the timeout turn: the scheduler must observe the new
    /// backoff deadline instead of emitting a catch-up request immediately.
    pub(crate) fn prepare_lookup(
        &mut self,
        fingers: &[Option<Did>],
        first_slot: usize,
        now_ms: u64,
        request_id: uuid::Uuid,
    ) -> Option<FingerFixRequest> {
        if fingers.is_empty() {
            return None;
        }
        if self.attempt.is_expired(now_ms) {
            self.attempt.clear();
            self.retry.record_failure(now_ms);
            return None;
        }
        if !self.attempt.is_idle()
            || self.evidence.all_verified()
            || !self.retry.permits_issue(now_ms)
        {
            return None;
        }

        // `first_slot` may skip the local successor interval; evidence decides
        // the next unverified slot at or after that boundary.
        let slot = self.evidence.first_unverified_from(first_slot)?;
        let request = FingerFixRequest::new(slot, request_id)?;
        self.attempt = FingerAttempt::awaiting_report(
            request,
            self.evidence.epoch(),
            now_ms.saturating_add(FINGER_LOOKUP_TIMEOUT_MS),
        );
        self.retry.record_issue(now_ms);
        Some(request)
    }

    /// Retain a timely report while the transport admits its candidate.
    pub(crate) fn defer_result(
        &mut self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerDeferOutcome {
        let proof = match self.validated_proof(local, slot_count, request, successor, now_ms) {
            Ok(proof) => proof,
            Err(rejection) => {
                self.retire_rejected_current(request, successor, rejection, now_ms);
                return FingerDeferOutcome::Rejected(rejection);
            }
        };
        if !self.attempt.already_retains(proof) {
            self.attempt
                .retain_for_admission(proof, now_ms.saturating_add(FINGER_ADMISSION_TIMEOUT_MS));
        }
        FingerDeferOutcome::Deferred { end: proof.end }
    }

    /// Commit an authenticated report to every still-current slot it proves.
    pub(crate) fn apply_result(
        &mut self,
        local: Did,
        fingers: &mut [Option<Did>],
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerApplyOutcome {
        let proof = match self.validated_proof(local, fingers.len(), request, successor, now_ms) {
            Ok(proof) => proof,
            Err(rejection) => {
                self.retire_rejected_current(request, successor, rejection, now_ms);
                return FingerApplyOutcome::Rejected(rejection);
            }
        };
        self.attempt.clear_if_owned(request);
        if self.evidence.apply(fingers, local, proof) {
            self.retry.record_progress();
            FingerApplyOutcome::Applied { end: proof.end }
        } else {
            FingerApplyOutcome::Rejected(FingerReportRejection::Stale)
        }
    }

    /// Retire a current, valid report whose candidate transport cannot use.
    ///
    /// Candidate validation and ownership retirement are one pure transition.
    /// In particular, a duplicate carrying the current UUID but a different
    /// successor cannot cancel a proof already retained for admission.
    pub(crate) fn retire_result(
        &mut self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerRetireOutcome {
        if let Err(rejection) = self.validated_proof(local, slot_count, request, successor, now_ms)
        {
            self.retire_rejected_current(request, successor, rejection, now_ms);
            return FingerRetireOutcome::Rejected(rejection);
        }

        // `validated_proof` established exact ownership of this token and, for
        // an admission proof, this successor. Retiring it is therefore safe.
        self.attempt.clear();
        self.retry.record_failure(now_ms);
        FingerRetireOutcome::Retired
    }

    /// Accept equivalent evidence produced by another local topology step.
    pub(crate) fn confirm_range(&mut self, start: usize, end: usize) -> bool {
        let newly_verified = self.evidence.confirm_range(start, end);
        // A local proof of the active slot supersedes the network lookup just
        // like a committed report would; it is progress, not cancellation.
        let superseded_attempt = self.attempt.clear_if_slot_in(start, end);
        let progressed = newly_verified || superseded_attempt;
        if progressed {
            self.retry.record_progress();
        }
        progressed
    }

    /// Cancel only the exact attempt owned by `request`.
    pub(crate) fn cancel(&mut self, request: FingerFixRequest, now_ms: u64) {
        if self.attempt.clear_if_owned(request) {
            self.retry.record_failure(now_ms);
        }
    }

    /// Validate token ownership first, then the Chord range and evidence epoch.
    fn validated_proof(
        &self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> Result<FingerRangeProof, FingerReportRejection> {
        match self.attempt.proof_source(request, successor, now_ms)? {
            // Admission proofs already passed Chord geometry and epoch checks
            // when they were retained, so only token/successor/expiry had to be
            // rechecked by `proof_source`.
            FingerProofSource::Admission(proof) => Ok(proof),
            FingerProofSource::Report { issued_epoch } => {
                let end = finger_proof_end(local, successor, request.slot_index(), slot_count)
                    .ok_or(FingerReportRejection::Invalid)?;
                let proof = FingerRangeProof {
                    request,
                    issued_epoch,
                    successor,
                    end,
                };
                self.evidence
                    .accepts(proof)
                    .then_some(proof)
                    .ok_or(FingerReportRejection::Stale)
            }
        }
    }

    /// Retire a consumed current token. Reordering is not a network failure.
    fn retire_rejected_current(
        &mut self,
        request: FingerFixRequest,
        successor: Did,
        rejection: FingerReportRejection,
        now_ms: u64,
    ) {
        if !self.attempt.can_consume(request, successor) {
            return;
        }
        self.attempt.clear();
        if rejection.counts_as_failure() {
            self.retry.record_failure(now_ms);
        }
    }
}

#[cfg(test)]
impl FingerConvergenceState {
    /// Whether tests should continue driving convergence transitions.
    pub(crate) fn is_pending(&self) -> bool {
        !self.attempt.is_idle() || !self.evidence.all_verified()
    }

    /// Status helper for tests that do not model the scheduler clock boundary.
    pub(crate) fn status(&self) -> FingerConvergenceStatus {
        self.status_after(0, 0)
    }

    /// Lossless test projection of private state-machine fields.
    pub(crate) fn projection(&self) -> FingerConvergenceProjection {
        let (in_flight, deferred, deferred_expires_at_ms, expires_at_ms) =
            self.attempt.projection();
        let (last_issued_at_ms, failure_streak, retry_not_before_ms) = self.retry.projection();
        FingerConvergenceProjection {
            verified: self.evidence.verified(),
            in_flight,
            deferred,
            deferred_expires_at_ms,
            expires_at_ms,
            last_issued_at_ms,
            failure_streak,
            retry_not_before_ms,
        }
    }

    /// Force exactly one slot back to unverified and reserve it for tests.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn prepare_slot_for_test(
        &mut self,
        fingers: &[Option<Did>],
        slot: usize,
        now_ms: u64,
        request_id: uuid::Uuid,
    ) -> Option<FingerFixRequest> {
        self.evidence.fill_verified(true);
        if !self.evidence.set_slot_verified(slot, false) {
            return None;
        }
        self.attempt.clear();
        self.retry.clear_last_issue();
        self.prepare_lookup(fingers, 0, now_ms, request_id)
    }

    /// Return per-slot verification bits for assertions.
    pub(crate) fn verified_for_test(&self) -> Vec<bool> {
        self.evidence.verified()
    }

    /// Replace verification bits from the front of the table for tests.
    pub(crate) fn set_verified_for_test(&mut self, values: &[bool]) {
        self.evidence.set_verified(values);
    }

    /// Set every slot's verification bit for tests.
    pub(crate) fn fill_verified_for_test(&mut self, verified: bool) {
        self.evidence.fill_verified(verified);
    }

    /// Set one slot's verification bit for tests, returning false if missing.
    pub(crate) fn set_slot_verified_for_test(&mut self, slot: usize, verified: bool) -> bool {
        self.evidence.set_slot_verified(slot, verified)
    }
}
