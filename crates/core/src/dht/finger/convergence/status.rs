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
    AwaitingReport { remaining_ms: u64 },
    /// One timely proof is waiting for candidate transport admission.
    AwaitingAdmission { remaining_ms: u64 },
}

/// Minimal scheduler-facing view of convergence state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceStatus {
    phase: FingerConvergencePhase,
    failure_streak: u8,
}

impl FingerConvergenceStatus {
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

    pub(crate) const fn awaiting_report(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingReport { remaining_ms },
            failure_streak,
        }
    }

    pub(crate) const fn awaiting_admission(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingAdmission { remaining_ms },
            failure_streak,
        }
    }

    pub(crate) const fn inactive() -> Self {
        Self::new(false, 0)
    }

    #[cfg(test)]
    pub(crate) const fn pending(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
    }

    pub(crate) const fn phase(self) -> FingerConvergencePhase {
        self.phase
    }

    pub(crate) const fn may_advance(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
    }

    pub(crate) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }
}
