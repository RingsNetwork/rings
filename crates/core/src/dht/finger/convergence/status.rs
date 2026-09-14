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
//! active attempt?
//!   | no                    | report             | admission
//!   v                       v                    v
//! unverified work?     remaining report ms  remaining admission ms
//!   | yes    | no            |                    |
//!   v        v               v                    v
//! Runnable Inactive   AwaitingReport       AwaitingAdmission
//!        \___________________|____________________/
//!                            |
//!                            v
//!              scheduler reads may_advance + streak
//! ```

/// Scheduler-visible phase of one node's finger convergence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerConvergencePhase {
    /// No automatic work is currently required.
    ///
    /// All routable evidence is verified and no request or admission lease owns
    /// outstanding work.
    Inactive,
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
    /// Build either `Runnable` or `Inactive` from the pending-evidence bit.
    ///
    /// `pending` describes unverified routable evidence when no attempt owns
    /// work. The supplied failure streak is preserved in either phase.
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
    ///
    /// `remaining_ms` is already saturated against the caller's clock and is
    /// stored unchanged with the current retry failure streak.
    pub(crate) const fn awaiting_report(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingReport { remaining_ms },
            failure_streak,
        }
    }

    /// Build status for a proof retained during transport admission.
    ///
    /// This phase prevents the scheduler from issuing another lookup while the
    /// transport owns a bounded decision window.
    pub(crate) const fn awaiting_admission(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingAdmission { remaining_ms },
            failure_streak,
        }
    }

    /// Build an inactive zero-failure status for absent finger tables.
    ///
    /// Callers use this neutral value when no convergence state exists; it
    /// reports neither runnable work nor historical retry pressure.
    pub(crate) const fn inactive() -> Self {
        Self::new(false, 0)
    }

    /// Test helper for whether any convergence phase may still run.
    ///
    /// Awaiting phases count as pending even though they cannot issue immediately,
    /// because model execution must still drive their completion or expiry.
    #[cfg(test)]
    pub(crate) const fn pending(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
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
    /// Runnable and awaiting phases return true; only the fixed-point
    /// `Inactive` phase returns false.
    pub(crate) const fn may_advance(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
    }

    /// Return consecutive current-attempt failures since last progress.
    ///
    /// The scheduler uses this value only for pacing policy; the convergence
    /// state machine remains the sole owner of failure updates.
    pub(crate) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }
}
