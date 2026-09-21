//! Pure state machine for range-aware Chord finger convergence.
//!
//! This module coordinates four independent concerns and deliberately owns no
//! network, lock, clock, or random-number effects:
//!
//! - `proof` derives which consecutive slots one Chord lookup proves;
//! - `evidence` versions the knowledge attached to each local finger hint;
//! - `attempt` gives one lookup exactly one ownership phase;
//! - `retry` records the deterministic lower bound for the next emission.
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
//! 5. Slot selection is cyclic: the next lookup starts after the range the
//!    previous attempt proved or failed, wrapping to the first routable slot.
//!    One slot whose successor never admits therefore costs one attempt per
//!    rotation instead of monopolizing the single attempt forever.
//!
//! # Algorithm flow
//!
//! ```text
//! topology hint change -> version affected evidence -> retire invalid owner
//!                                                        |
//! scheduler tick -> expire old owner -> apply retry floor |
//!        |                                               |
//!        v                                               |
//! choose next unverified routable slot after the cursor  |
//!        |                                               |
//!        v                                               |
//! issue (slot, UUID, evidence epoch)                     |
//!        |                                               |
//!        v                                               |
//! validate token -> deadline -> geometry -> epoch        |
//!        | reject                                        |
//!        +------------------------> retire/backoff       |
//!        | accept                                        |
//!        v                                               |
//! apply now OR retain for admission                      |
//!        |                                               |
//!        v                                               |
//! verify still-current range -> clear retry pressure ----+
//! ```

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

use self::attempt::FingerAttempt;
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
///
/// Once this process-monotonic interval elapses, the request is retired and a
/// retry failure is recorded before another lookup may be emitted.
const FINGER_LOOKUP_TIMEOUT_MS: u64 = 10_000;

/// Maximum time a timely proof may wait for transport admission.
///
/// The lease is a backstop for a proof that never gains a connection
/// generation to own it (the handler died between deferring the proof and
/// reserving the handshake). A proof that is attached to a generation is
/// released by that generation's own expiry or admission, so the lease must
/// outlast a full handshake generation: it is the handshake timeout plus a
/// margin covering the gap between report arrival (when this lease starts, on
/// the ring's monotonic clock) and handshake reservation (when the generation's
/// wall-clock timeout starts). Expiry counts as a failed attempt and therefore
/// enters exponential backoff.
pub(crate) const FINGER_ADMISSION_TIMEOUT_MS: u64 = 210_000;

/// Serializable protocol state for one node's local finger convergence.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FingerConvergenceState {
    /// Per-slot verification bits plus the hint-change epoch they belong to.
    ///
    /// A report may update a slot only when its captured epoch is at least the
    /// slot's last-change epoch, which prevents stale hint restoration.
    evidence: FingerEvidence,
    /// The one lookup or admission proof currently owned by this state.
    ///
    /// Its enum form makes report waiting and admission waiting mutually
    /// exclusive ownership phases.
    attempt: FingerAttempt,
    /// Deterministic retry floor applied before the scheduler adds jitter.
    ///
    /// It tracks failures and minimum issue spacing but contains no timer or
    /// random-number side effect.
    retry: FingerRetryState,
    /// Slot after the range the last attempt proved or failed.
    ///
    /// [`Self::prepare_lookup`] resumes selection here (cyclically), so the
    /// unverified slots are served round-robin rather than lowest-first, and
    /// [`Self::begin_revalidation`] reopens the run that starts here, so
    /// revalidation and selection walk the table in the same direction. It is
    /// the only cursor the table has.
    cursor: usize,
}

/// Test-only observation used by the retry and interleaving models.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceProjection {
    /// Snapshot of each slot's verified bit in table order.
    ///
    /// Its length equals the normalized finger-table width.
    pub(crate) verified: Vec<bool>,
    /// Request waiting for a successor report.
    ///
    /// Present only while the attempt is in the report-waiting phase.
    pub(crate) in_flight: Option<FingerFixRequest>,
    /// Request whose proof is held while transport admission finishes.
    ///
    /// Mutually exclusive with `in_flight` because one attempt owns one phase.
    pub(crate) deferred: Option<FingerFixRequest>,
    /// Admission lease deadline for `deferred`, when present.
    ///
    /// Model tests compare this process-monotonic value with simulated time.
    pub(crate) deferred_expires_at_ms: Option<u64>,
    /// Report deadline for `in_flight`, when present.
    ///
    /// Absent outside the report-waiting phase.
    pub(crate) expires_at_ms: Option<u64>,
    /// Monotonic timestamp of the last emitted automatic lookup.
    ///
    /// It witnesses enforcement of the hard minimum issue interval.
    pub(crate) last_issued_at_ms: Option<u64>,
    /// Number of consecutive current-attempt failures since last progress.
    ///
    /// Stale reports do not increment it; accepted evidence resets it.
    pub(crate) failure_streak: u8,
    /// Earliest deterministic retry timestamp after failures.
    ///
    /// Scheduler jitter is intentionally excluded from this pure projection.
    pub(crate) retry_not_before_ms: Option<u64>,
    /// Slot at which selection and revalidation resume.
    pub(crate) cursor: usize,
}

impl FingerConvergenceState {
    /// Create fully unverified convergence state for a table width.
    ///
    /// `slot_count` fixes the evidence-vector width. The result owns no request
    /// and has no retry history, so its first eligible tick may select work.
    pub(crate) fn new(slot_count: usize) -> Self {
        Self {
            evidence: FingerEvidence::new(slot_count),
            attempt: FingerAttempt::Idle,
            retry: FingerRetryState::default(),
            cursor: 0,
        }
    }

    /// Convert internal evidence and attempt ownership into scheduler state.
    ///
    /// `first_routable_slot` excludes the local successor range already proved
    /// by stabilization, so a node does not keep issuing Chord lookups for
    /// slots whose target is known to resolve locally.
    /// `now_ms` computes a saturating remaining lease without mutating state.
    pub(crate) fn status_after(
        &self,
        first_routable_slot: usize,
        now_ms: u64,
    ) -> FingerConvergenceStatus {
        FingerConvergenceStatus::new(
            self.attempt.phase(
                now_ms,
                self.evidence.any_unverified_from(first_routable_slot),
            ),
            self.retry.failure_streak(),
        )
    }

    /// Invalidate only evidence whose inferred finger hint changed.
    ///
    /// A change at the active request's lower slot destroys the premise of its
    /// range proof, so that attempt is retired. Changes elsewhere are recorded
    /// by epoch and will be skipped if an older range result later arrives.
    /// The snapshots are compared only across the normalized evidence width.
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
    ///
    /// The transition advances evidence freshness, marks every slot unverified,
    /// and releases ownership. It is idempotent at the idle, unverified state.
    pub(crate) fn invalidate_all_evidence(&mut self) {
        if self.attempt.is_idle() && self.evidence.all_unverified() {
            return;
        }
        self.evidence.invalidate_all();
        self.attempt.clear();
    }

    /// Reopen one consecutive hint range for periodic revalidation.
    ///
    /// Revalidation reopens the run of equal hints that starts at the cursor,
    /// the slot after the last range an attempt proved or failed, wrapping
    /// once. It is deferred while an attempt is active or while routable work
    /// (at or after `first_routable_slot`) is still pending and succeeding.
    /// Pending work that is failing (`failure_streak > 0`) does not defer it:
    /// otherwise one slot whose successor never admits would block
    /// revalidation of every other range for as long as it keeps failing.
    pub(crate) fn begin_revalidation(
        &mut self,
        fingers: &[Option<Did>],
        first_routable_slot: usize,
    ) {
        let succeeding_work_pending = self.evidence.any_unverified_from(first_routable_slot)
            && self.retry.failure_streak() == 0;
        if self.attempt.is_idle() && !succeeding_work_pending {
            self.evidence.reopen_next_range(fingers, self.cursor);
        }
    }

    /// Reserve the next lookup if ownership, evidence, and pacing allow it.
    ///
    /// Timeout processing happens before reservation. This call intentionally
    /// returns `None` on the timeout turn: the scheduler must observe the new
    /// backoff deadline instead of emitting a catch-up request immediately.
    /// Success captures the current epoch, owns the returned token, and records
    /// `now_ms` as an emission.
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
            if let Some(request) = self.attempt.request() {
                self.fail_attempt(fingers, request.slot_index(), now_ms);
            }
            return None;
        }
        if !self.attempt.is_idle()
            || self.evidence.all_verified()
            || !self.retry.permits_issue(now_ms)
        {
            return None;
        }

        // `first_slot` may skip the local successor interval; evidence decides
        // the next unverified slot at or after that boundary, resuming after
        // the previous attempt so no single slot can starve the rest.
        let slot = self
            .evidence
            .next_unverified_cyclic(first_slot, self.cursor)?;
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
    ///
    /// The exact token, deadline, Chord geometry, and current evidence are
    /// validated first. Success starts a bounded lease without editing hints.
    pub(crate) fn defer_result(
        &mut self,
        local: Did,
        fingers: &[Option<Did>],
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerDeferOutcome {
        let proof = match self.validated_proof(local, fingers.len(), request, successor, now_ms) {
            Ok(proof) => proof,
            Err(rejection) => {
                self.retire_rejected_current(fingers, request, successor, rejection, now_ms);
                return FingerDeferOutcome::Rejected(rejection);
            }
        };
        if !self.attempt.already_retains(proof) {
            self.attempt
                .retain_for_admission(proof, now_ms.saturating_add(FINGER_ADMISSION_TIMEOUT_MS));
        }
        FingerDeferOutcome::Deferred
    }

    /// Commit an authenticated report to every still-current slot it proves.
    ///
    /// Eligible slots receive the successor and become verified; slots changed
    /// after issuance are skipped. Any committed slot resets retry pressure.
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
                self.retire_rejected_current(fingers, request, successor, rejection, now_ms);
                return FingerApplyOutcome::Rejected(rejection);
            }
        };
        self.attempt.clear_if_owned(request);
        // `validated_proof` established that the proof is accepted by at least
        // one slot of its range, so application always commits.
        self.evidence.apply(fingers, local, proof);
        self.cursor = proof.end.saturating_add(1);
        self.retry.record_progress();
        FingerApplyOutcome::Applied
    }

    /// Retire a current, valid report whose candidate transport cannot use.
    ///
    /// Candidate validation and ownership retirement are one pure transition.
    /// In particular, a duplicate carrying the current UUID but a different
    /// successor cannot cancel a proof already retained for admission.
    /// A valid current candidate records failure because transport could not use
    /// it, while stale conflicting input leaves ownership unchanged.
    pub(crate) fn retire_result(
        &mut self,
        local: Did,
        fingers: &[Option<Did>],
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerRetireOutcome {
        let proof = match self.validated_proof(local, fingers.len(), request, successor, now_ms) {
            Ok(proof) => proof,
            Err(rejection) => {
                self.retire_rejected_current(fingers, request, successor, rejection, now_ms);
                return FingerRetireOutcome::Rejected(rejection);
            }
        };

        // `validated_proof` established exact ownership of this token and, for
        // an admission proof, this successor. Retiring it is therefore safe.
        // The whole proved range failed with this candidate, so selection
        // resumes after it.
        self.attempt.clear();
        self.cursor = proof.end.saturating_add(1);
        self.retry.record_failure(now_ms);
        FingerRetireOutcome::Retired
    }

    /// Accept equivalent evidence produced by another local topology step.
    ///
    /// The inclusive range is clamped to table width. New proof bits or a
    /// superseded request count as progress and clear retry pressure. A
    /// superseded attempt also moves the cursor past the confirmed range, as
    /// its own applied proof would have; an untouched attempt keeps the
    /// cursor where it is, so the rotation is not restarted from the low end.
    pub(crate) fn confirm_range(&mut self, start: usize, end: usize) -> bool {
        let newly_verified = self.evidence.confirm_range(start, end);
        // A local proof of the active slot supersedes the network lookup just
        // like a committed report would; it is progress, not cancellation.
        let superseded_attempt = self.attempt.clear_if_slot_in(start, end);
        if superseded_attempt {
            self.cursor = end.saturating_add(1);
        }
        let progressed = newly_verified || superseded_attempt;
        if progressed {
            self.retry.record_progress();
        }
        progressed
    }

    /// Cancel only the exact attempt owned by `request`.
    ///
    /// Matching ownership is released and counted as a failure at `now_ms`.
    /// Stale tokens are no-ops and cannot retire newer work.
    pub(crate) fn cancel(
        &mut self,
        fingers: &[Option<Did>],
        request: FingerFixRequest,
        now_ms: u64,
    ) {
        if self.attempt.clear_if_owned(request) {
            self.fail_attempt(fingers, request.slot_index(), now_ms);
        }
    }

    /// Record one failed attempt at `slot` and move selection past its hint run.
    ///
    /// The slots sharing `slot`'s inferred hint expect the same successor, so
    /// retrying them next would repeat the same failure; the cursor skips the
    /// run and the retry floor rises.
    fn fail_attempt(&mut self, fingers: &[Option<Did>], slot: usize, now_ms: u64) {
        self.attempt.clear();
        self.cursor = FingerEvidence::hint_run_end(fingers, slot).saturating_add(1);
        self.retry.record_failure(now_ms);
    }

    /// Validate token ownership first, then the Chord range and evidence epoch.
    ///
    /// Fresh reports become geometric range proofs. Retained admission proofs
    /// skip repeated geometry work but still require exact ownership and lease.
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
    ///
    /// Only a matching token and successor may clear the phase. Invalid or
    /// expired current input adds retry pressure; stale input is ignored.
    fn retire_rejected_current(
        &mut self,
        fingers: &[Option<Did>],
        request: FingerFixRequest,
        successor: Did,
        rejection: FingerReportRejection,
        now_ms: u64,
    ) {
        if !self.attempt.can_consume(request, successor) {
            return;
        }
        if rejection.counts_as_failure() {
            self.fail_attempt(fingers, request.slot_index(), now_ms);
        } else {
            self.attempt.clear();
        }
    }
}

#[cfg(test)]
impl FingerConvergenceState {
    /// Whether tests should continue driving convergence transitions.
    ///
    /// The result remains true while an attempt owns work or any slot lacks
    /// proof, so model tests stop only at the true fixed point.
    pub(crate) fn is_pending(&self) -> bool {
        !self.attempt.is_idle() || !self.evidence.all_verified()
    }

    /// Status helper for tests that do not model the scheduler clock boundary.
    ///
    /// It projects from slot zero at time zero while preserving production phase
    /// selection and avoiding a wall-clock dependency.
    pub(crate) fn status(&self) -> FingerConvergenceStatus {
        self.status_after(0, 0)
    }

    /// Lossless test projection of private state-machine fields.
    ///
    /// Algebraic ownership and retry state are flattened into comparable values
    /// without exposing mutable production internals.
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
            cursor: self.cursor,
        }
    }

    /// Place the selection and revalidation cursor for a test fixture.
    pub(crate) fn set_cursor_for_test(&mut self, cursor: usize) {
        self.cursor = cursor;
    }

    /// Force exactly one slot back to unverified and reserve it for tests.
    ///
    /// Other slots become verified, ownership and issue spacing are cleared, and
    /// the real preparation path runs. An invalid slot returns `None`.
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
    ///
    /// The copy preserves table order and cannot mutate production evidence.
    pub(crate) fn verified_for_test(&self) -> Vec<bool> {
        self.evidence.verified()
    }

    /// Replace verification bits from the front of the table for tests.
    ///
    /// Slots beyond `values` are reset to unverified for deterministic fixtures.
    pub(crate) fn set_verified_for_test(&mut self, values: &[bool]) {
        self.evidence.set_verified(values);
    }

    /// Set every slot's verification bit for tests.
    ///
    /// Epochs, ownership, and retry state remain unchanged.
    pub(crate) fn fill_verified_for_test(&mut self, verified: bool) {
        self.evidence.fill_verified(verified);
    }

    /// Set one slot's verification bit for tests, returning false if missing.
    ///
    /// A valid index updates one bit; an invalid index is an explicit no-op.
    pub(crate) fn set_slot_verified_for_test(&mut self, slot: usize, verified: bool) -> bool {
        self.evidence.set_slot_verified(slot, verified)
    }
}
