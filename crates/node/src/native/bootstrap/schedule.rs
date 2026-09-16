//! Pure retry schedule for managed bootstrap targets.
//!
//! The schedule is a total function of explicit monotonic time (`now_ms`): it owns no timers,
//! performs no IO, and draws jitter from a seeded generator, so the supervisor shell can replay
//! any turn sequence deterministically. One [`TargetPhase`] per managed target, keyed by the
//! target's DID, evolves under three explicit transitions, `begin`, `settle` and
//! `notice_loss`, plus the passage of time that makes a phase *due*:
//!
//! ```text
//!                begin                              settle(Reachable) ∧ ¬lost
//!  ┌───────────┐ ──────────▶ ┌───────────────┐ ───────────────────────────▶ ┌────────────────┐
//!  │  Pending  │             │     Busy      │                              │   Reachable    │
//!  │  f, t₀    │ ◀────────── │   f, lost     │ ◀─────────────────────────── │   recheck_at   │
//!  └───────────┘  settle     └───────────────┘   begin  (recheck_at ≤ now)  └────────────────┘
//!     ▲    ▲     (DialFailed)       │                                              │
//!     │    │     f ↦ f + 1          │ settle(Reachable) ∧ lost                     │ notice_loss
//!     │    │     t₀ ↦ now+delay(f+1)│ f ↦ 0, t₀ ↦ now                              │ f ↦ 0, t₀ ↦ now
//!     │    │     settle(Deferred)   │                                              │
//!     │    │     t₀ ↦ now+BURST_DELAY                                              │
//!     │    └────────────────────────┴──────────────────────────────────────────────┘
//!     └── notice_loss: f ↦ 0, t₀ ↦ now   (the target was admitted since, and left again)
//!
//!  due(Pending)   ⟺ t₀ ≤ now        due(Reachable) ⟺ recheck_at ≤ now        due(Busy) = ⊥
//!  delay(k)       = BURST_DELAY                          if k < BURST_ATTEMPTS  (rapid burst)
//!                 = BASE_INTERVAL + U[0, JITTER_WINDOW]  otherwise              (slow cadence)
//! ```
//!
//! Laws:
//!
//! - *No overlap*: `begin` is the only entry into `Busy`, and a `Busy` target is never due, so
//!   at most one turn per target is in flight. Precondition of `begin`: the target is due; the
//!   shell only calls it on the output of `due`.
//! - *Bounded burst*: after the k-th consecutive failure the next attempt waits `delay(k)`, so
//!   the first `BURST_ATTEMPTS` attempts are at least `BURST_DELAY` apart (measured from the
//!   settlement of the previous attempt) and every later attempt is at least `BASE_INTERVAL`
//!   apart.
//! - *Deferral is not failure*: a turn that found a handshake to the target already in flight
//!   waits `BURST_DELAY` and keeps its failure count.
//! - *Reset on success*: `Reachable` carries no failure count; a later loss restarts the burst.
//! - *Losses are never lost*: a loss noticed while `Busy` turns the next `Reachable` settlement
//!   into an immediate `Pending`; a loss noticed while `Pending` (the target was admitted by
//!   stabilization meanwhile and left again) restarts the burst at once.
//! - *Totality*: every transition on a non-matching phase or unknown DID is the identity and
//!   reports `false`.

use std::collections::BTreeMap;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use rings_core::dht::Did;

/// Attempts in the rapid burst that follows a loss of reachability.
pub(crate) const BURST_ATTEMPTS: u8 = 5;
/// Delay between consecutive attempts inside the rapid burst, and after a deferred turn.
pub(crate) const BURST_DELAY: Duration = Duration::from_secs(2);
/// Base cadence once the burst is exhausted, and the recheck period of a reachable target.
pub(crate) const BASE_INTERVAL: Duration = Duration::from_secs(300);
/// Inclusive upper bound of the uniform jitter added to every `BASE_INTERVAL` delay, so a fleet
/// that lost the same target at the same instant does not redial it in lockstep.
pub(crate) const JITTER_WINDOW: Duration = Duration::from_secs(30);

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
    /// A turn is in flight; `lost` records a departure of the target noticed meanwhile.
    Busy {
        /// Consecutive failed dials carried into this turn.
        failures: u8,
        /// Whether the target left the local DHT while the turn ran.
        lost: bool,
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
    /// The target was reachable through the overlay, or the redial completed with admission.
    Reachable,
    /// A handshake to the target was already in flight; nothing was attempted.
    Deferred,
    /// The target was unreachable and the redial failed.
    DialFailed,
}

/// The jittered delay law over a seeded generator.
#[derive(Debug)]
struct Jitter(StdRng);

impl Jitter {
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
        let jitter_ms = self.0.gen_range(0..=duration_ms(JITTER_WINDOW));
        duration_ms(BASE_INTERVAL).saturating_add(jitter_ms)
    }
}

/// Retry schedule over every managed target.
#[derive(Debug)]
pub(crate) struct BootstrapSchedule {
    phases: BTreeMap<Did, TargetPhase>,
    jitter: Jitter,
}

impl BootstrapSchedule {
    /// A schedule in which each of `targets` is due immediately, with jitter drawn from a
    /// generator seeded by `jitter_seed`.
    pub(crate) fn new(targets: impl IntoIterator<Item = Did>, jitter_seed: u64) -> Self {
        Self {
            phases: targets
                .into_iter()
                .map(|target| {
                    (target, TargetPhase::Pending {
                        failures: 0,
                        not_before_ms: 0,
                    })
                })
                .collect(),
            jitter: Jitter(StdRng::seed_from_u64(jitter_seed)),
        }
    }

    /// Current phase of `target`, or `None` for a DID outside the target set.
    #[cfg(test)]
    pub(crate) fn phase(&self, target: Did) -> Option<TargetPhase> {
        self.phases.get(&target).copied()
    }

    /// Targets whose phase is due at `now_ms`, in DID order.
    pub(crate) fn due(&self, now_ms: u64) -> impl Iterator<Item = Did> + '_ {
        self.phases
            .iter()
            .filter(move |(_, phase)| phase.is_due(now_ms))
            .map(|(target, _)| *target)
    }

    /// Enter `Busy` for `target`; `false` when the target is unknown or already busy.
    pub(crate) fn begin(&mut self, target: Did) -> bool {
        let Some(phase) = self.phases.get_mut(&target) else {
            return false;
        };
        let failures = match *phase {
            TargetPhase::Pending { failures, .. } => failures,
            TargetPhase::Reachable { .. } => 0,
            TargetPhase::Busy { .. } => return false,
        };
        *phase = TargetPhase::Busy {
            failures,
            lost: false,
        };
        true
    }

    /// Leave `Busy` for `target` with `outcome` at `now_ms`; `false` when no turn was in flight.
    pub(crate) fn settle(&mut self, target: Did, outcome: TurnOutcome, now_ms: u64) -> bool {
        let Some(phase) = self.phases.get_mut(&target) else {
            return false;
        };
        let TargetPhase::Busy { failures, lost } = *phase else {
            return false;
        };
        *phase = match outcome {
            TurnOutcome::Reachable if lost => TargetPhase::Pending {
                failures: 0,
                not_before_ms: now_ms,
            },
            TurnOutcome::Reachable => TargetPhase::Reachable {
                recheck_at_ms: now_ms.saturating_add(self.jitter.slow_delay_ms()),
            },
            TurnOutcome::Deferred => TargetPhase::Pending {
                failures,
                not_before_ms: now_ms.saturating_add(duration_ms(BURST_DELAY)),
            },
            TurnOutcome::DialFailed => {
                let failures = failures.saturating_add(1);
                TargetPhase::Pending {
                    failures,
                    not_before_ms: now_ms.saturating_add(self.jitter.delay_ms(failures)),
                }
            }
        };
        true
    }

    /// Record at `now_ms` that `target` left the local DHT; `false` when the phase already
    /// accounts for it or the DID is unknown.
    pub(crate) fn notice_loss(&mut self, target: Did, now_ms: u64) -> bool {
        let Some(phase) = self.phases.get_mut(&target) else {
            return false;
        };
        match *phase {
            TargetPhase::Reachable { .. } | TargetPhase::Pending { .. } => {
                *phase = TargetPhase::Pending {
                    failures: 0,
                    not_before_ms: now_ms,
                };
                true
            }
            TargetPhase::Busy {
                failures,
                lost: false,
            } => {
                *phase = TargetPhase::Busy {
                    failures,
                    lost: true,
                };
                true
            }
            TargetPhase::Busy { lost: true, .. } => false,
        }
    }

    /// Earliest instant at which some target becomes due, or `None` while every target is busy
    /// or the set is empty.
    pub(crate) fn next_deadline_ms(&self) -> Option<u64> {
        self.phases
            .values()
            .copied()
            .filter_map(TargetPhase::deadline_ms)
            .min()
    }
}

impl TargetPhase {
    /// Whether this phase is due at `now_ms`.
    fn is_due(self, now_ms: u64) -> bool {
        self.deadline_ms()
            .is_some_and(|deadline| deadline <= now_ms)
    }

    /// The instant this phase becomes due, or `None` while busy.
    fn deadline_ms(self) -> Option<u64> {
        match self {
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

/// Whether `instant_ms` lies exactly one slow-cadence delay after `from_ms`, that is within
/// `[from + BASE_INTERVAL, from + BASE_INTERVAL + JITTER_WINDOW]`.
#[cfg(test)]
pub(crate) fn is_one_slow_delay_after(from_ms: u64, instant_ms: u64) -> bool {
    let floor = from_ms.saturating_add(duration_ms(BASE_INTERVAL));
    (floor..=floor.saturating_add(duration_ms(JITTER_WINDOW))).contains(&instant_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single target of most tests.
    const TARGET: u32 = 1;

    /// A schedule over `count` targets with DIDs `1..=count`.
    fn schedule(count: u32) -> BootstrapSchedule {
        BootstrapSchedule::new((1..=count).map(Did::from), 1)
    }

    /// Run one failed turn for the target at `now_ms` and return the resulting `not_before_ms`.
    fn fail_turn(schedule: &mut BootstrapSchedule, now_ms: u64) -> u64 {
        let target = Did::from(TARGET);
        assert!(schedule.begin(target));
        assert!(schedule.settle(target, TurnOutcome::DialFailed, now_ms));
        match schedule.phase(target) {
            Some(TargetPhase::Pending { not_before_ms, .. }) => not_before_ms,
            other => panic!("failed turn must leave the target pending, got {other:?}"),
        }
    }

    /// A fresh schedule makes every target due at instant zero.
    #[test]
    fn every_target_is_due_at_start() {
        let schedule = schedule(3);
        assert_eq!(schedule.due(0).collect::<Vec<_>>(), vec![
            Did::from(1),
            Did::from(2),
            Did::from(3)
        ]);
        assert_eq!(schedule.next_deadline_ms(), Some(0));
    }

    /// Bounded-burst law: attempts 1..BURST_ATTEMPTS wait BURST_DELAY, later ones one slow delay.
    #[test]
    fn burst_attempts_are_two_seconds_apart_then_slow_down() {
        let mut schedule = schedule(1);
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
        assert!(is_one_slow_delay_after(now_ms, slow));
        let slower = fail_turn(&mut schedule, slow);
        assert!(is_one_slow_delay_after(slow, slower));
    }

    /// Deferral law: a deferred turn waits BURST_DELAY and keeps its failure count.
    #[test]
    fn a_deferred_turn_keeps_its_failure_count() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for _ in 0..3 {
            now_ms = fail_turn(&mut schedule, now_ms);
        }
        assert!(schedule.begin(target));
        assert!(schedule.settle(target, TurnOutcome::Deferred, now_ms));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Pending {
                failures: 3,
                not_before_ms: now_ms + 2_000
            })
        );
    }

    /// Reset-on-success law: a reachable settlement forgets failures and a loss restarts the burst.
    #[test]
    fn success_resets_the_failure_count() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for _ in 0..3 {
            now_ms = fail_turn(&mut schedule, now_ms);
        }
        assert!(schedule.begin(target));
        assert!(schedule.settle(target, TurnOutcome::Reachable, now_ms));
        let Some(TargetPhase::Reachable { recheck_at_ms }) = schedule.phase(target) else {
            panic!("a reachable outcome must settle into Reachable");
        };
        assert!(is_one_slow_delay_after(now_ms, recheck_at_ms));
        assert!(schedule.notice_loss(target, recheck_at_ms - 1));
        assert_eq!(
            schedule.phase(target),
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

    /// A loss noticed while pending in the slow cadence restarts the burst at once.
    #[test]
    fn a_loss_while_pending_restarts_the_burst() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for _ in 0..BURST_ATTEMPTS {
            now_ms = fail_turn(&mut schedule, now_ms);
        }
        assert!(matches!(
            schedule.phase(target),
            Some(TargetPhase::Pending { failures: 5, .. })
        ));
        assert!(schedule.notice_loss(target, 20));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Pending {
                failures: 0,
                not_before_ms: 20
            })
        );
    }

    /// A reachable target is due exactly at its recheck instant and begins with zero failures.
    #[test]
    fn a_reachable_target_is_rechecked_when_its_deadline_passes() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        assert!(schedule.begin(target));
        assert!(schedule.settle(target, TurnOutcome::Reachable, 0));
        let deadline = schedule
            .next_deadline_ms()
            .expect("a reachable target has a deadline");
        assert!(schedule.due(deadline - 1).next().is_none());
        assert_eq!(schedule.due(deadline).collect::<Vec<_>>(), vec![target]);
        assert!(schedule.begin(target));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Busy {
                failures: 0,
                lost: false
            })
        );
    }

    /// Losses-are-never-lost law: a loss noticed while busy makes the reachable settlement pending now.
    #[test]
    fn a_loss_during_a_turn_forces_an_immediate_reassessment() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        assert!(schedule.begin(target));
        assert!(schedule.notice_loss(target, 5));
        assert!(
            !schedule.notice_loss(target, 6),
            "a second loss is already accounted for"
        );
        assert!(schedule.settle(target, TurnOutcome::Reachable, 10));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Pending {
                failures: 0,
                not_before_ms: 10
            })
        );
    }

    /// No-overlap law: a busy target cannot begin again, is never due, and has no deadline.
    #[test]
    fn a_busy_target_is_neither_due_nor_restartable() {
        let mut schedule = schedule(2);
        assert!(schedule.begin(Did::from(1)));
        assert!(!schedule.begin(Did::from(1)));
        assert_eq!(schedule.due(u64::MAX).collect::<Vec<_>>(), vec![Did::from(
            2
        )]);
        assert!(schedule.begin(Did::from(2)));
        assert_eq!(schedule.next_deadline_ms(), None);
    }

    /// Totality law: mismatched phases and unknown DIDs are identities reporting `false`.
    #[test]
    fn transitions_are_total_over_unknown_dids_and_phases() {
        let unknown = Did::from(7);
        let mut schedule = schedule(1);
        assert!(!schedule.begin(unknown));
        assert!(!schedule.settle(unknown, TurnOutcome::Reachable, 0));
        assert!(!schedule.notice_loss(unknown, 0));
        assert!(!schedule.settle(Did::from(TARGET), TurnOutcome::Reachable, 0));
        assert_eq!(schedule.phase(unknown), None);
    }

    /// Equal seeds draw equal slow delays, every one inside `[BASE_INTERVAL, BASE_INTERVAL + JITTER_WINDOW]`.
    #[test]
    fn jitter_is_deterministic_per_seed_and_bounded() {
        let mut left = BootstrapSchedule::new([Did::from(TARGET)], 42);
        let mut right = BootstrapSchedule::new([Did::from(TARGET)], 42);
        for _ in 0..8 {
            let left_delay = left.jitter.slow_delay_ms();
            let right_delay = right.jitter.slow_delay_ms();
            assert_eq!(left_delay, right_delay);
            assert!(is_one_slow_delay_after(0, left_delay));
        }
    }

    /// The failure count saturates at `u8::MAX` instead of wrapping.
    #[test]
    fn failure_count_saturates() {
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for _ in 0..(u8::MAX as usize + 2) {
            now_ms = fail_turn(&mut schedule, now_ms);
        }
        assert!(matches!(
            schedule.phase(Did::from(TARGET)),
            Some(TargetPhase::Pending {
                failures: u8::MAX,
                ..
            })
        ));
    }
}
