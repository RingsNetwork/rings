//! Pure retry schedule for managed bootstrap targets.
//!
//! The schedule is a total function of explicit monotonic time (`now_ms`): it owns no timers,
//! performs no IO, and draws jitter from a seeded generator, so the supervisor shell can replay
//! any turn sequence deterministically. One [`TargetPhase`] per managed target evolves under
//! three explicit transitions, `begin`, `settle` and `notice_drop`, plus the passage of time
//! that makes a phase *due*:
//!
//! ```text
//!                begin                              settle(Reachable) ∧ ¬dropped
//!  ┌───────────┐ ──────────▶ ┌───────────────┐ ───────────────────────────▶ ┌────────────────┐
//!  │  Pending  │             │     Busy      │                              │   Reachable    │
//!  │  f, t₀    │ ◀────────── │  f, dropped   │ ◀─────────────────────────── │   recheck_at   │
//!  └───────────┘  settle     └───────────────┘   begin  (recheck_at ≤ now)  └────────────────┘
//!        ▲        (DialFailed)       │                                              │
//!        │        f ↦ f + 1          │ settle(Reachable) ∧ dropped                  │ notice_drop
//!        │        t₀ ↦ now+delay(f+1)│ f ↦ 0, t₀ ↦ now                              │ f ↦ 0, t₀ ↦ now
//!        └───────────────────────────┴──────────────────────────────────────────────┘
//!
//!  due(Pending)   ⟺ t₀ ≤ now        due(Reachable) ⟺ recheck_at ≤ now        due(Busy) = ⊥
//!  delay(k)       = BURST_DELAY                          if k < BURST_ATTEMPTS  (rapid burst)
//!                 = BASE_INTERVAL + U[0, JITTER_WINDOW]  otherwise              (slow cadence)
//! ```
//!
//! Laws:
//!
//! - *No overlap*: `begin` is the only entry into `Busy`, and a `Busy` target is never due, so
//!   at most one turn per target is in flight.
//! - *Bounded burst*: after the k-th consecutive failure the next attempt waits `delay(k)`, so
//!   the first `BURST_ATTEMPTS` attempts are `BURST_DELAY` apart and every later attempt is at
//!   least `BASE_INTERVAL` apart.
//! - *Reset on success*: `Reachable` carries no failure count; a later loss restarts the burst.
//! - *Drops are never lost*: a drop noticed while `Busy` turns the next `Reachable` settlement
//!   into an immediate `Pending`, so the turn that raced the loss is reassessed at once.
//! - *Totality*: every transition on a non-matching phase or unknown index is the identity and
//!   reports `false`.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

/// Attempts in the rapid burst that follows a loss of reachability.
pub(crate) const BURST_ATTEMPTS: u8 = 5;
/// Delay between consecutive attempts inside the rapid burst.
pub(crate) const BURST_DELAY: Duration = Duration::from_secs(2);
/// Base cadence once the burst is exhausted, and the recheck period of a reachable target.
pub(crate) const BASE_INTERVAL: Duration = Duration::from_secs(300);
/// Inclusive upper bound of the uniform jitter added to every `BASE_INTERVAL` delay, so a fleet
/// that lost the same target at the same instant does not redial it in lockstep.
pub(crate) const JITTER_WINDOW: Duration = Duration::from_secs(30);

/// Position of a managed target in the supervisor's target list.
pub(crate) type TargetIndex = usize;

/// Lifecycle phase of one managed target; see the module diagram.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetPhase {
    /// Waiting for its next turn after `failures` consecutive failed dials; due once
    /// `not_before_ms` has passed.
    Pending {
        /// Consecutive failed dials since the target was last reachable.
        failures: u8,
        /// Earliest instant of the next turn.
        not_before_ms: u64,
    },
    /// A turn is in flight; `dropped` records a transport loss noticed meanwhile.
    Busy {
        /// Consecutive failed dials carried into this turn.
        failures: u8,
        /// Whether the direct transport was lost while the turn ran.
        dropped: bool,
    },
    /// The last turn found the target reachable; reassessed once `recheck_at_ms` has passed.
    Reachable {
        /// Instant of the next periodic reassessment.
        recheck_at_ms: u64,
    },
}

/// Result of one supervisor turn, as reported by the shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnOutcome {
    /// The target was reachable through the overlay, or the redial handshake completed.
    Reachable,
    /// The target was unreachable and the redial failed.
    DialFailed,
}

/// Retry schedule over every managed target.
#[derive(Debug)]
pub(crate) struct BootstrapSchedule {
    phases: Vec<TargetPhase>,
    jitter: StdRng,
}

impl BootstrapSchedule {
    /// A schedule in which each of `targets` targets is due immediately, with jitter drawn from
    /// a generator seeded by `jitter_seed`.
    pub(crate) fn new(targets: usize, jitter_seed: u64) -> Self {
        Self {
            phases: vec![
                TargetPhase::Pending {
                    failures: 0,
                    not_before_ms: 0,
                };
                targets
            ],
            jitter: StdRng::seed_from_u64(jitter_seed),
        }
    }

    /// Current phase of `index`, or `None` for an index outside the target list.
    pub(crate) fn phase(&self, index: TargetIndex) -> Option<TargetPhase> {
        self.phases.get(index).copied()
    }

    /// Targets whose phase is due at `now_ms`, in index order.
    pub(crate) fn due(&self, now_ms: u64) -> impl Iterator<Item = TargetIndex> + '_ {
        self.phases
            .iter()
            .enumerate()
            .filter(move |(_, phase)| phase.is_due(now_ms))
            .map(|(index, _)| index)
    }

    /// Enter `Busy` for `index`; `false` when the target is unknown or already busy.
    pub(crate) fn begin(&mut self, index: TargetIndex) -> bool {
        let Some(phase) = self.phases.get_mut(index) else {
            return false;
        };
        let failures = match *phase {
            TargetPhase::Pending { failures, .. } => failures,
            TargetPhase::Reachable { .. } => 0,
            TargetPhase::Busy { .. } => return false,
        };
        *phase = TargetPhase::Busy {
            failures,
            dropped: false,
        };
        true
    }

    /// Leave `Busy` for `index` with `outcome` at `now_ms`; `false` when no turn was in flight.
    pub(crate) fn settle(&mut self, index: TargetIndex, outcome: TurnOutcome, now_ms: u64) -> bool {
        let Some(TargetPhase::Busy { failures, dropped }) = self.phase(index) else {
            return false;
        };
        let next = match outcome {
            TurnOutcome::Reachable if dropped => TargetPhase::Pending {
                failures: 0,
                not_before_ms: now_ms,
            },
            TurnOutcome::Reachable => TargetPhase::Reachable {
                recheck_at_ms: now_ms.saturating_add(self.slow_delay_ms()),
            },
            TurnOutcome::DialFailed => {
                let failures = failures.saturating_add(1);
                TargetPhase::Pending {
                    failures,
                    not_before_ms: now_ms.saturating_add(self.delay_ms(failures)),
                }
            }
        };
        self.phases
            .get_mut(index)
            .map(|phase| *phase = next)
            .is_some()
    }

    /// Record at `now_ms` that the direct transport to `index` was lost; `false` when the
    /// phase already accounts for it.
    pub(crate) fn notice_drop(&mut self, index: TargetIndex, now_ms: u64) -> bool {
        let Some(phase) = self.phases.get_mut(index) else {
            return false;
        };
        match *phase {
            TargetPhase::Reachable { .. } => {
                *phase = TargetPhase::Pending {
                    failures: 0,
                    not_before_ms: now_ms,
                };
                true
            }
            TargetPhase::Busy {
                failures,
                dropped: false,
            } => {
                *phase = TargetPhase::Busy {
                    failures,
                    dropped: true,
                };
                true
            }
            TargetPhase::Busy { dropped: true, .. } | TargetPhase::Pending { .. } => false,
        }
    }

    /// Earliest instant at which some target becomes due, or `None` while every target is busy
    /// or the list is empty.
    pub(crate) fn next_deadline_ms(&self) -> Option<u64> {
        self.phases
            .iter()
            .filter_map(TargetPhase::deadline_ms)
            .min()
    }

    /// Delay before the attempt that follows the `failures`-th consecutive failure.
    fn delay_ms(&mut self, failures: u8) -> u64 {
        if failures < BURST_ATTEMPTS {
            duration_ms(BURST_DELAY)
        } else {
            self.slow_delay_ms()
        }
    }

    /// One slow-cadence delay: `BASE_INTERVAL` plus a uniform draw from `[0, JITTER_WINDOW]`.
    fn slow_delay_ms(&mut self) -> u64 {
        let jitter_ms = self.jitter.gen_range(0..=duration_ms(JITTER_WINDOW));
        duration_ms(BASE_INTERVAL).saturating_add(jitter_ms)
    }
}

impl TargetPhase {
    /// Whether this phase is due at `now_ms`.
    fn is_due(self, now_ms: u64) -> bool {
        self.deadline_ms()
            .is_some_and(|deadline| deadline <= now_ms)
    }

    /// The instant this phase becomes due, or `None` while busy.
    fn deadline_ms(&self) -> Option<u64> {
        match *self {
            Self::Pending { not_before_ms, .. } => Some(not_before_ms),
            Self::Reachable { recheck_at_ms } => Some(recheck_at_ms),
            Self::Busy { .. } => None,
        }
    }
}

/// Whole milliseconds of `duration`, saturating at `u64::MAX`.
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inclusive lower bound of one slow-cadence delay in milliseconds.
    const SLOW_MIN_MS: u64 = 300_000;
    /// Inclusive upper bound of one slow-cadence delay in milliseconds.
    const SLOW_MAX_MS: u64 = 330_000;

    /// Run one failed turn at `now_ms` and return the resulting `not_before_ms`.
    fn fail_turn(schedule: &mut BootstrapSchedule, now_ms: u64) -> u64 {
        assert!(schedule.begin(0));
        assert!(schedule.settle(0, TurnOutcome::DialFailed, now_ms));
        match schedule.phase(0) {
            Some(TargetPhase::Pending { not_before_ms, .. }) => not_before_ms,
            other => panic!("failed turn must leave the target pending, got {other:?}"),
        }
    }

    /// A fresh schedule makes every target due at instant zero.
    #[test]
    fn every_target_is_due_at_start() {
        let schedule = BootstrapSchedule::new(3, 1);
        assert_eq!(schedule.due(0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(schedule.next_deadline_ms(), Some(0));
    }

    /// Bounded-burst law: attempts 1..BURST_ATTEMPTS wait BURST_DELAY, later ones one slow delay.
    #[test]
    fn burst_attempts_are_two_seconds_apart_then_slow_down() {
        let mut schedule = BootstrapSchedule::new(1, 1);
        let mut now_ms = 0;
        for attempt in 1..BURST_ATTEMPTS {
            let next = fail_turn(&mut schedule, now_ms);
            assert_eq!(
                next,
                now_ms + 2_000,
                "attempt {attempt} must wait BURST_DELAY"
            );
            now_ms = next;
        }
        let slow = fail_turn(&mut schedule, now_ms);
        assert!((now_ms + SLOW_MIN_MS..=now_ms + SLOW_MAX_MS).contains(&slow));
        let slower = fail_turn(&mut schedule, slow);
        assert!((slow + SLOW_MIN_MS..=slow + SLOW_MAX_MS).contains(&slower));
    }

    /// Reset-on-success law: a reachable settlement forgets failures and a drop restarts the burst.
    #[test]
    fn success_resets_the_failure_count() {
        let mut schedule = BootstrapSchedule::new(1, 1);
        let mut now_ms = 0;
        for _ in 0..3 {
            now_ms = fail_turn(&mut schedule, now_ms);
        }
        assert!(schedule.begin(0));
        assert!(schedule.settle(0, TurnOutcome::Reachable, now_ms));
        let Some(TargetPhase::Reachable { recheck_at_ms }) = schedule.phase(0) else {
            panic!("a reachable outcome must settle into Reachable");
        };
        assert!((now_ms + SLOW_MIN_MS..=now_ms + SLOW_MAX_MS).contains(&recheck_at_ms));
        assert!(schedule.notice_drop(0, recheck_at_ms - 1));
        assert_eq!(
            schedule.phase(0),
            Some(TargetPhase::Pending {
                failures: 0,
                not_before_ms: recheck_at_ms - 1
            })
        );
        assert_eq!(
            fail_turn(&mut schedule, recheck_at_ms),
            recheck_at_ms + 2_000
        );
    }

    /// A reachable target is due exactly at its recheck instant and begins with zero failures.
    #[test]
    fn a_reachable_target_is_rechecked_when_its_deadline_passes() {
        let mut schedule = BootstrapSchedule::new(1, 1);
        assert!(schedule.begin(0));
        assert!(schedule.settle(0, TurnOutcome::Reachable, 0));
        let deadline = schedule
            .next_deadline_ms()
            .expect("a reachable target has a deadline");
        assert!(schedule.due(deadline - 1).next().is_none());
        assert_eq!(schedule.due(deadline).collect::<Vec<_>>(), vec![0]);
        assert!(schedule.begin(0));
        assert_eq!(
            schedule.phase(0),
            Some(TargetPhase::Busy {
                failures: 0,
                dropped: false
            })
        );
    }

    /// Drops-are-never-lost law: a drop noticed while busy makes the reachable settlement pending now.
    #[test]
    fn a_drop_during_a_turn_forces_an_immediate_reassessment() {
        let mut schedule = BootstrapSchedule::new(1, 1);
        assert!(schedule.begin(0));
        assert!(schedule.notice_drop(0, 5));
        assert!(
            !schedule.notice_drop(0, 6),
            "a second drop is already accounted for"
        );
        assert!(schedule.settle(0, TurnOutcome::Reachable, 10));
        assert_eq!(
            schedule.phase(0),
            Some(TargetPhase::Pending {
                failures: 0,
                not_before_ms: 10
            })
        );
    }

    /// No-overlap law: a busy target cannot begin again, is never due, and has no deadline.
    #[test]
    fn a_busy_target_is_neither_due_nor_restartable() {
        let mut schedule = BootstrapSchedule::new(2, 1);
        assert!(schedule.begin(0));
        assert!(!schedule.begin(0));
        assert_eq!(schedule.due(u64::MAX).collect::<Vec<_>>(), vec![1]);
        assert!(schedule.begin(1));
        assert_eq!(schedule.next_deadline_ms(), None);
    }

    /// Totality law: mismatched phases and unknown indices are identities reporting `false`.
    #[test]
    fn transitions_are_total_over_unknown_indices_and_phases() {
        let mut schedule = BootstrapSchedule::new(1, 1);
        assert!(!schedule.begin(7));
        assert!(!schedule.settle(7, TurnOutcome::Reachable, 0));
        assert!(!schedule.notice_drop(7, 0));
        assert!(!schedule.settle(0, TurnOutcome::Reachable, 0));
        assert!(
            !schedule.notice_drop(0, 0),
            "a pending target already awaits a turn"
        );
        assert_eq!(schedule.phase(7), None);
    }

    /// Equal seeds draw equal slow delays, every one inside `[BASE_INTERVAL, BASE_INTERVAL + JITTER_WINDOW]`.
    #[test]
    fn jitter_is_deterministic_per_seed_and_bounded() {
        let mut left = BootstrapSchedule::new(1, 42);
        let mut right = BootstrapSchedule::new(1, 42);
        for _ in 0..8 {
            let l = left.slow_delay_ms();
            let r = right.slow_delay_ms();
            assert_eq!(l, r);
            assert!((SLOW_MIN_MS..=SLOW_MAX_MS).contains(&l));
        }
    }

    /// The failure count saturates at `u8::MAX` instead of wrapping.
    #[test]
    fn failure_count_saturates() {
        let mut schedule = BootstrapSchedule::new(1, 1);
        let mut now_ms = 0;
        for _ in 0..(u8::MAX as usize + 2) {
            now_ms = fail_turn(&mut schedule, now_ms);
        }
        assert!(matches!(
            schedule.phase(0),
            Some(TargetPhase::Pending {
                failures: u8::MAX,
                ..
            })
        ));
    }
}
