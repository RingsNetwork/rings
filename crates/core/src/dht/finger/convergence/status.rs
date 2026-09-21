//! Read-only scheduler projection of finger convergence.
//!
//! Runtime scheduling uses remaining durations rather than absolute monotonic
//! timestamps. A restarted listener has a new clock origin, so exporting an
//! absolute deadline would make the same lookup wait twice for the node age.
//!
//! # Algorithm flow
//!
//! ```text
//! convergence evidence + attempt + retry streak
//!        |
//!        v
//! successor head?  -- no --> Dormant
//!   | yes
//!   v
//! active attempt?
//!   | no                    | report             | admission
//!   v                       v                    v
//! unverified work?     remaining report ms  remaining admission ms
//!   | yes    | no            |                    |
//!   v        v               v                    v
//! Runnable Converged  AwaitingReport       AwaitingAdmission
//!        \___________________|____________________/
//!                            |
//!                            v
//!              scheduler reads may_advance + streak
//! ```

/// Scheduler-visible phase of one node's finger convergence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerConvergencePhase {
    /// The node has no successor head, so no range can be proved.
    ///
    /// Leaving this phase is activation: the first successor was admitted and
    /// the scheduler spreads the node's first attempt over the fleet-start
    /// window.
    Dormant,
    /// Every routable slot is verified and nothing owns outstanding work.
    ///
    /// Periodic revalidation reopens ranges from here; leaving this phase is
    /// ordinary continuation, paced by the retry delay, not activation.
    Converged,
    /// An unverified range can issue a lookup when pacing permits.
    ///
    /// The scheduler must still honor minimum spacing, failure backoff, and its
    /// own jitter before requesting a transition.
    Runnable,
    /// One emitted lookup is waiting for its report.
    ///
    /// Additional lookup emission is suppressed until ownership is consumed or
    /// the remaining duration reaches zero.
    AwaitingReport {
        /// Saturating duration until the report token expires.
        ///
        /// The scheduler may sleep for this duration without depending on the
        /// state machine's process-local absolute clock origin.
        remaining_ms: u64,
    },
    /// One timely proof is waiting for candidate transport admission.
    ///
    /// The Chord proof has already been validated, but hint mutation waits for
    /// the outer transport layer to accept the candidate.
    AwaitingAdmission {
        /// Saturating duration until the retained proof expires.
        ///
        /// Zero tells the caller to drive expiry; it cannot underflow even when
        /// the modeled clock has advanced beyond the deadline.
        remaining_ms: u64,
    },
}

/// Minimal scheduler-facing view of convergence state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceStatus {
    /// Current scheduler phase, expressed without absolute deadlines.
    ///
    /// This projection intentionally omits proof contents and request tokens so
    /// the scheduler cannot mutate protocol ownership.
    phase: FingerConvergencePhase,
    /// Consecutive failure count used by scheduler jitter policy.
    ///
    /// The count accompanies every phase, allowing the outer scheduler to size
    /// jitter without reading private retry state.
    failure_streak: u8,
}

impl FingerConvergenceStatus {
    /// Pair a phase with the retry failure streak it was observed under.
    pub(crate) const fn new(phase: FingerConvergencePhase, failure_streak: u8) -> Self {
        Self {
            phase,
            failure_streak,
        }
    }

    /// Build either `Runnable` or `Converged` from the pending-evidence bit.
    ///
    /// `pending` describes unverified routable evidence when no attempt owns
    /// work. The supplied failure streak is preserved in either phase.
    #[cfg(test)]
    pub(crate) const fn idle(pending: bool, failure_streak: u8) -> Self {
        Self::new(
            if pending {
                FingerConvergencePhase::Runnable
            } else {
                FingerConvergencePhase::Converged
            },
            failure_streak,
        )
    }

    /// Build status for a lookup that is still waiting on a report.
    #[cfg(test)]
    pub(crate) const fn awaiting_report(remaining_ms: u64, failure_streak: u8) -> Self {
        Self::new(
            FingerConvergencePhase::AwaitingReport { remaining_ms },
            failure_streak,
        )
    }

    /// Build status for a proof retained during transport admission.
    #[cfg(test)]
    pub(crate) const fn awaiting_admission(remaining_ms: u64, failure_streak: u8) -> Self {
        Self::new(
            FingerConvergencePhase::AwaitingAdmission { remaining_ms },
            failure_streak,
        )
    }

    /// Build the dormant status of a node without a successor head.
    ///
    /// The value carries no failure history: a node that cannot prove any
    /// range has nothing to back off from.
    pub(crate) const fn dormant() -> Self {
        Self {
            phase: FingerConvergencePhase::Dormant,
            failure_streak: 0,
        }
    }

    /// Return the scheduler phase without exposing internal convergence state.
    ///
    /// The enum is copied out and carries only relative durations, never request
    /// ownership or process-local absolute timestamps.
    pub(crate) const fn phase(self) -> FingerConvergencePhase {
        self.phase
    }

    /// Return whether polling this status can produce useful finger work.
    ///
    /// Runnable and awaiting phases return true; the two quiescent phases,
    /// `Dormant` and `Converged`, return false.
    pub(crate) const fn may_advance(self) -> bool {
        !matches!(
            self.phase,
            FingerConvergencePhase::Dormant | FingerConvergencePhase::Converged
        )
    }

    /// Return consecutive current-attempt failures since last progress.
    ///
    /// The scheduler uses this value only for pacing policy; the convergence
    /// state machine remains the sole owner of failure updates.
    pub(crate) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }
}
