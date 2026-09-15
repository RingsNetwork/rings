//! Ownership state for one finger lookup and its optional admission lease.
//!
//! The state graph is deliberately algebraic:
//!
//! `Idle -> AwaitingReport -> AwaitingAdmission -> Idle`
//!
//! `AwaitingReport -> Idle` is also allowed for direct application, timeout,
//! cancellation, or topology invalidation. There is no representation in
//! which a lookup and an admission proof are active simultaneously.
//!
//! # Algorithm flow
//!
//! ```text
//! Idle
//!   | emit request(slot, UUID, epoch, deadline)
//!   v
//! AwaitingReport
//!   |---- timeout / cancel / invalidation ----------------------> Idle
//!   | validate exact token and deadline
//!   v
//! Report proof source
//!   |---- direct commit ----------------------------------------> Idle
//!   | retain validated proof and admission deadline
//!   v
//! AwaitingAdmission
//!   |---- admit / retire / timeout -----------------------------> Idle
//!   +---- mismatched duplicate ----------------------------> unchanged
//! ```

use serde::Deserialize;
use serde::Serialize;

use super::proof::FingerFixRequest;
use super::proof::FingerRangeProof;
use super::proof::FingerReportRejection;
use crate::dht::Did;

/// Lookup ownership while the query is waiting for its successor report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct PendingFingerLookup {
    /// Correlation token emitted with the lookup.
    ///
    /// Both its slot and UUID must match before a report can consume ownership.
    request: FingerFixRequest,
    /// Evidence epoch observed when the lookup was issued.
    ///
    /// Later hint changes use this snapshot to reject stale slot updates.
    issued_epoch: u64,
    /// Monotonic deadline after which the report is no longer current.
    ///
    /// It is compared only against caller-supplied process-monotonic time.
    expires_at_ms: u64,
}

/// Validated proof retained while transport admission decides usability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct AdmissionFingerProof {
    /// Range proof already checked against Chord geometry and evidence epochs.
    ///
    /// The value binds admission to its request, successor, epoch, and range.
    proof: FingerRangeProof,
    /// Monotonic deadline for assigning the candidate to a connection.
    ///
    /// Expiry releases ownership and becomes retry pressure in the outer state.
    expires_at_ms: u64,
}

/// Exactly one transport-independent ownership phase for finger convergence.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) enum FingerAttempt {
    /// No lookup or retained proof is owned by the state machine.
    #[default]
    Idle,
    /// A lookup was emitted and is waiting for its correlated report.
    AwaitingReport(
        /// Owned report-phase data binding the request to its issue epoch and
        /// process-monotonic deadline.
        PendingFingerLookup,
    ),
    /// A timely report is retained while its candidate is admitted.
    AwaitingAdmission(
        /// Owned admission-phase data binding a validated proof to its bounded
        /// transport decision window.
        AdmissionFingerProof,
    ),
}

/// Attempt information needed to validate a reported successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FingerProofSource {
    /// A fresh report needs the Chord range lemma applied to it.
    ///
    /// Ownership and expiry are checked, but geometry and evidence remain for
    /// the caller to validate.
    Report {
        /// Evidence epoch captured when the lookup left the state machine.
        ///
        /// This value bounds the hint revisions the resulting proof may update.
        issued_epoch: u64,
    },
    /// A previously validated report already owns its proved range.
    ///
    /// It may be committed after exact admission ownership and deadline checks.
    Admission(
        /// Complete retained range proof whose request and successor already
        /// matched the active admission lease.
        FingerRangeProof,
    ),
}

/// Scheduler-facing projection of the active attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FingerAttemptStatus {
    /// No request or admission lease exists.
    ///
    /// Emission still depends on evidence and retry pacing outside this enum.
    Idle,
    /// A lookup report has not arrived yet.
    ///
    /// Additional lookups are suppressed until this phase is consumed or expires.
    AwaitingReport {
        /// Saturating duration until the report deadline.
        ///
        /// Zero signals expiry without allowing arithmetic underflow.
        remaining_ms: u64,
    },
    /// A report was validated and is waiting for transport admission.
    ///
    /// No hint is committed until the outer transport accepts the candidate.
    AwaitingAdmission {
        /// Saturating duration until the admission lease deadline.
        ///
        /// Zero signals expiry without exposing an absolute clock origin.
        remaining_ms: u64,
    },
}

impl FingerAttempt {
    /// Return true only when no token owns convergence work.
    ///
    /// This pure phase predicate does not inspect evidence, pacing, or expiry.
    pub(super) const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Return the token that owns the active phase, if any.
    ///
    /// Admission ownership returns the request embedded in its retained proof.
    pub(super) const fn request(self) -> Option<FingerFixRequest> {
        match self {
            Self::Idle => None,
            Self::AwaitingReport(pending) => Some(pending.request),
            Self::AwaitingAdmission(admission) => Some(admission.proof.request),
        }
    }

    /// Whether this report is allowed to consume the current ownership phase.
    ///
    /// Before validation, the successor is not yet known and the exact token
    /// owns any geometrically valid answer. Once retained for admission, both
    /// token and successor are fixed; a conflicting duplicate must not evict
    /// the retained proof.
    /// Expiry, geometry, and evidence are intentionally checked elsewhere.
    pub(super) fn can_consume(self, request: FingerFixRequest, successor: Did) -> bool {
        match self {
            Self::Idle => false,
            Self::AwaitingReport(pending) => pending.request == request,
            Self::AwaitingAdmission(admission) => {
                admission.proof.request == request && admission.proof.successor == successor
            }
        }
    }

    /// Construct a newly emitted lookup phase.
    ///
    /// The epoch snapshots evidence freshness, while `expires_at_ms` is the
    /// absolute process-monotonic report deadline supplied by the caller.
    pub(super) const fn awaiting_report(
        request: FingerFixRequest,
        issued_epoch: u64,
        expires_at_ms: u64,
    ) -> Self {
        Self::AwaitingReport(PendingFingerLookup {
            request,
            issued_epoch,
            expires_at_ms,
        })
    }

    /// Drop or clamp restored ownership state after a table-width change.
    ///
    /// Out-of-range requests and reversed ranges are discarded. A valid
    /// admission proof keeps its lower slot and clamps its upper slot.
    pub(super) fn normalize(&mut self, slot_count: usize) {
        let Some(request) = self.request() else {
            return;
        };
        if request.slot_index() >= slot_count {
            *self = Self::Idle;
            return;
        }
        if let Self::AwaitingAdmission(admission) = self {
            if admission.proof.end < request.slot_index() {
                *self = Self::Idle;
                return;
            }
            admission.proof.end = admission.proof.end.min(slot_count.saturating_sub(1));
        }
    }

    /// Return the active phase and its remaining lease from `now_ms`.
    ///
    /// Saturating subtraction projects overdue phases as zero without mutating
    /// or implicitly expiring ownership.
    pub(super) const fn status(self, now_ms: u64) -> FingerAttemptStatus {
        match self {
            Self::Idle => FingerAttemptStatus::Idle,
            Self::AwaitingReport(pending) => FingerAttemptStatus::AwaitingReport {
                remaining_ms: pending.expires_at_ms.saturating_sub(now_ms),
            },
            Self::AwaitingAdmission(admission) => FingerAttemptStatus::AwaitingAdmission {
                remaining_ms: admission.expires_at_ms.saturating_sub(now_ms),
            },
        }
    }

    /// Return whether the active phase has reached its monotonic deadline.
    ///
    /// Idle never expires; an active phase expires at or after its deadline.
    pub(super) const fn is_expired(self, now_ms: u64) -> bool {
        match self {
            Self::Idle => false,
            Self::AwaitingReport(pending) => now_ms >= pending.expires_at_ms,
            Self::AwaitingAdmission(admission) => now_ms >= admission.expires_at_ms,
        }
    }

    /// Resolve the current token into either raw report data or retained proof.
    ///
    /// Ownership is decided by [`Self::can_consume`] (for `AwaitingReport` the
    /// successor is not matched, since this is the first place a reported
    /// successor can be evaluated; for `AwaitingAdmission` it is part of the
    /// retained proof) and expiry by [`Self::is_expired`]. Mismatches return
    /// `Stale`, an owned but overdue phase returns `Expired`; neither changes
    /// this value.
    pub(super) fn proof_source(
        self,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> Result<FingerProofSource, FingerReportRejection> {
        if !self.can_consume(request, successor) {
            return Err(FingerReportRejection::Stale);
        }
        if self.is_expired(now_ms) {
            return Err(FingerReportRejection::Expired);
        }
        match self {
            Self::AwaitingReport(pending) => Ok(FingerProofSource::Report {
                issued_epoch: pending.issued_epoch,
            }),
            Self::AwaitingAdmission(admission) => Ok(FingerProofSource::Admission(admission.proof)),
            // `can_consume` is false for `Idle`.
            Self::Idle => Err(FingerReportRejection::Stale),
        }
    }

    /// Replace lookup ownership with an admission lease for the same proof.
    ///
    /// The proof must already satisfy token, geometry, and evidence validation.
    /// This transition stores no transport handle and performs no side effect.
    pub(super) fn retain_for_admission(&mut self, proof: FingerRangeProof, expires_at_ms: u64) {
        *self = Self::AwaitingAdmission(AdmissionFingerProof {
            proof,
            expires_at_ms,
        });
    }

    /// Return true when a duplicate admission path is already holding `proof`.
    ///
    /// Equality covers token, epoch, successor, and range end, so a conflicting
    /// duplicate cannot share ownership.
    pub(super) fn already_retains(self, proof: FingerRangeProof) -> bool {
        matches!(
            self,
            Self::AwaitingAdmission(admission) if admission.proof == proof
        )
    }

    /// Release any active ownership phase.
    ///
    /// Evidence and retry state are untouched; the outer convergence transition
    /// decides whether release represents failure or progress.
    pub(super) fn clear(&mut self) {
        *self = Self::Idle;
    }

    /// Release ownership only if `request` matches the active token.
    ///
    /// The return value is true exactly when a phase was cleared. Stale tokens
    /// are no-ops and cannot cancel newer work.
    pub(super) fn clear_if_owned(&mut self, request: FingerFixRequest) -> bool {
        if self.request() == Some(request) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// Release ownership if its lower slot has been verified elsewhere.
    ///
    /// The inclusive range acts as equivalent local proof. The result tells the
    /// caller whether active ownership was superseded.
    pub(super) fn clear_if_slot_in(&mut self, start: usize, end: usize) -> bool {
        if self
            .request()
            .is_some_and(|request| (start..=end).contains(&request.slot_index()))
        {
            self.clear();
            true
        } else {
            false
        }
    }

    /// Project active ownership into simple fields for state-machine tests.
    ///
    /// Report and admission fields cannot both be populated, preserving phase
    /// exclusivity while exposing copied values only.
    #[cfg(test)]
    pub(super) const fn projection(
        self,
    ) -> (
        Option<FingerFixRequest>,
        Option<FingerFixRequest>,
        Option<u64>,
        Option<u64>,
    ) {
        match self {
            Self::Idle => (None, None, None, None),
            Self::AwaitingReport(pending) => (
                Some(pending.request),
                None,
                None,
                Some(pending.expires_at_ms),
            ),
            Self::AwaitingAdmission(admission) => (
                None,
                Some(admission.proof.request),
                Some(admission.expires_at_ms),
                None,
            ),
        }
    }
}
