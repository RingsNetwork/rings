//! Read-only scheduler projection of finger convergence.
//!
//! Runtime scheduling uses remaining durations rather than absolute monotonic
//! timestamps. A restarted listener has a new clock origin, so exporting an
//! absolute deadline would make the same lookup wait twice for the node age.

/// Scheduler-visible phase of one node's finger convergence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerConvergencePhase {
    /// No automatic work is currently required.
    Inactive,
    /// An unverified range can issue a lookup when pacing permits.
    Runnable,
    /// One emitted lookup is waiting for its report.
    AwaitingReport {
        /// Saturating duration until the report token expires.
        remaining_ms: u64,
    },
    /// One timely proof is waiting for candidate transport admission.
    AwaitingAdmission {
        /// Saturating duration until the retained proof expires.
        remaining_ms: u64,
    },
}

/// Minimal scheduler-facing view of convergence state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceStatus {
    /// Current scheduler phase, expressed without absolute deadlines.
    phase: FingerConvergencePhase,
    /// Consecutive failure count used by scheduler jitter policy.
    failure_streak: u8,
}

impl FingerConvergenceStatus {
    /// Build either `Runnable` or `Inactive` from the pending-evidence bit.
    pub(crate) const fn new(pending: bool, failure_streak: u8) -> Self {
        Self {
            phase: if pending {
                FingerConvergencePhase::Runnable
            } else {
                FingerConvergencePhase::Inactive
            },
            failure_streak,
        }
    }

    /// Build status for a lookup that is still waiting on a report.
    pub(crate) const fn awaiting_report(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingReport { remaining_ms },
            failure_streak,
        }
    }

    /// Build status for a proof retained during transport admission.
    pub(crate) const fn awaiting_admission(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingAdmission { remaining_ms },
            failure_streak,
        }
    }

    /// Build an inactive zero-failure status for absent finger tables.
    pub(crate) const fn inactive() -> Self {
        Self::new(false, 0)
    }

    /// Test helper for whether any convergence phase may still run.
    #[cfg(test)]
    pub(crate) const fn pending(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
    }

    /// Scheduler phase without exposing internal convergence structures.
    pub(crate) const fn phase(self) -> FingerConvergencePhase {
        self.phase
    }

    /// Whether polling this status can produce useful finger work.
    pub(crate) const fn may_advance(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
    }

    /// Consecutive current-attempt failures since last progress.
    pub(crate) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }
}
