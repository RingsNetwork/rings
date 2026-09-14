//! Ownership state for one finger lookup and its optional admission lease.
//!
//! The state graph is deliberately algebraic:
//!
//! `Idle -> AwaitingReport -> AwaitingAdmission -> Idle`
//!
//! `AwaitingReport -> Idle` is also allowed for direct application, timeout,
//! cancellation, or topology invalidation. There is no representation in
//! which a lookup and an admission proof are active simultaneously.

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
    request: FingerFixRequest,
    /// Evidence epoch observed when the lookup was issued.
    issued_epoch: u64,
    /// Monotonic deadline after which the report is no longer current.
    expires_at_ms: u64,
}

/// Validated proof retained while transport admission decides usability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct AdmissionFingerProof {
    /// Range proof already checked against Chord geometry and evidence epochs.
    proof: FingerRangeProof,
    /// Monotonic deadline for assigning the candidate to a connection.
    expires_at_ms: u64,
}

/// Exactly one transport-independent ownership phase for finger convergence.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) enum FingerAttempt {
    /// No lookup or retained proof is owned by the state machine.
    #[default]
    Idle,
    /// A lookup was emitted and is waiting for its correlated report.
    AwaitingReport(PendingFingerLookup),
    /// A timely report is retained while its candidate is admitted.
    AwaitingAdmission(AdmissionFingerProof),
}

/// Attempt information needed to validate a reported successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FingerProofSource {
    /// A fresh report needs the Chord range lemma applied to it.
    Report {
        /// Evidence epoch captured when the lookup left the state machine.
        issued_epoch: u64,
    },
    /// A previously validated report already owns its proved range.
    Admission(FingerRangeProof),
}

/// Scheduler-facing projection of the active attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FingerAttemptStatus {
    /// No request or admission lease exists.
    Idle,
    /// A lookup report has not arrived yet.
    AwaitingReport {
        /// Saturating duration until the report deadline.
        remaining_ms: u64,
    },
    /// A report was validated and is waiting for transport admission.
    AwaitingAdmission {
        /// Saturating duration until the admission lease deadline.
        remaining_ms: u64,
    },
}

impl FingerAttempt {
    /// Return true only when no token owns convergence work.
    pub(super) const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Return the token that owns the active phase, if any.
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
    pub(super) const fn is_expired(self, now_ms: u64) -> bool {
        match self {
            Self::Idle => false,
            Self::AwaitingReport(pending) => now_ms >= pending.expires_at_ms,
            Self::AwaitingAdmission(admission) => now_ms >= admission.expires_at_ms,
        }
    }

    /// Resolve the current token into either raw report data or retained proof.
    ///
    /// For `AwaitingReport`, the successor is intentionally not matched here:
    /// this is the first place a reported successor can be evaluated. For
    /// `AwaitingAdmission`, the successor is part of the retained proof and
    /// must match exactly before the caller may consume the lease.
    pub(super) fn proof_source(
        self,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> Result<FingerProofSource, FingerReportRejection> {
        match self {
            Self::AwaitingReport(pending) if pending.request == request => {
                if now_ms >= pending.expires_at_ms {
                    Err(FingerReportRejection::Expired)
                } else {
                    Ok(FingerProofSource::Report {
                        issued_epoch: pending.issued_epoch,
                    })
                }
            }
            Self::AwaitingAdmission(admission)
                if admission.proof.request == request && admission.proof.successor == successor =>
            {
                if now_ms >= admission.expires_at_ms {
                    Err(FingerReportRejection::Expired)
                } else {
                    Ok(FingerProofSource::Admission(admission.proof))
                }
            }
            Self::Idle | Self::AwaitingReport(_) | Self::AwaitingAdmission(_) => {
                Err(FingerReportRejection::Stale)
            }
        }
    }

    /// Replace lookup ownership with an admission lease for the same proof.
    pub(super) fn retain_for_admission(&mut self, proof: FingerRangeProof, expires_at_ms: u64) {
        *self = Self::AwaitingAdmission(AdmissionFingerProof {
            proof,
            expires_at_ms,
        });
    }

    /// Return true when a duplicate admission path is already holding `proof`.
    pub(super) fn already_retains(self, proof: FingerRangeProof) -> bool {
        matches!(
            self,
            Self::AwaitingAdmission(admission) if admission.proof == proof
        )
    }

    /// Release any active ownership phase.
    pub(super) fn clear(&mut self) {
        *self = Self::Idle;
    }

    /// Release ownership only if `request` matches the active token.
    pub(super) fn clear_if_owned(&mut self, request: FingerFixRequest) -> bool {
        if self.request() == Some(request) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// Release ownership if its lower slot has been verified elsewhere.
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
