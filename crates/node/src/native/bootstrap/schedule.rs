//! Pure retry schedule for managed bootstrap targets.
//!
//! The schedule is a total function of explicit monotonic time (`now_ms`): it owns no timers,
//! performs no IO, and draws jitter from a seeded generator, so the supervisor shell can replay
//! any turn sequence deterministically. One [`TargetPhase`] per managed target, keyed by the
//! target's DID, evolves under three explicit transitions, `begin_if_due`, `settle` and
//! `notice_loss`, plus the passage of time that makes a phase *due*:
//!
//! ```text
//!               begin_if_due                       settle(Reachable) ∧ ¬lost
//!  ┌───────────┐ ──────────▶ ┌───────────────┐ ───────────────────────────▶ ┌────────────────┐
//!  │  Pending  │             │     Busy      │                              │   Reachable    │
//!  │  m, t₀    │ ◀────────── │   m, lost     │ ◀─────────────────────────── │   recheck_at   │
//!  └───────────┘  settle     └───────────────┘ begin_if_due (recheck_at ≤ now)└────────────────┘
//!     ▲    ▲     (Unreachable)      │                                              │
//!     │    │     ∧ ¬lost            │ settle(_) ∧ lost                             │ notice_loss
//!     │    │     m ↦ m + 1          │ m ↦ 0, t₀ ↦ now                              │ m ↦ 0, t₀ ↦ now
//!     │    │     t₀ ↦ now+delay(m+1)│                                              │
//!     │    └────────────────────────┴──────────────────────────────────────────────┘
//!     └── notice_loss: m ↦ 0, t₀ ↦ now   (the target was admitted since, and left again)
//!
//!  due(Pending)   ⟺ t₀ ≤ now        due(Reachable) ⟺ recheck_at ≤ now        due(Busy) = ⊥
//!  delay(k)       = BURST_DELAY                          if k < BURST_ATTEMPTS  (rapid burst)
//!                 = BASE_INTERVAL + U[0, JITTER_WINDOW]  otherwise              (slow cadence)
//! ```
//!
//! Laws:
//!
//! - *No overlap*: `begin_if_due` is the only entry into `Busy` and refuses a target that is not
//!   due, and a `Busy` target is never due, so at most one turn per target is in flight.
//! - *Bounded burst*: a target with `m` consecutive misses waits `delay(m)` before its next
//!   turn, measured from the settlement of the previous one, so a loss is followed by at most
//!   `BURST_ATTEMPTS` turns `BURST_DELAY` apart and then by turns `BASE_INTERVAL` apart. A miss
//!   is any turn that did not find the target reachable: a failed dial, or one refused because
//!   the target's slot is owned by another attempt; the budget counts turns, not causes, so a
//!   handshake the peer keeps in flight cannot hold the target in the rapid cadence.
//! - *Reset on success*: `Reachable` carries no miss count; a later loss restarts the burst.
//! - *Losses are never lost*: a loss makes the target pending at once with the burst restarted,
//!   whether it is noticed while `Reachable`, while `Pending` (the target was admitted by
//!   stabilization meanwhile and left again), or while `Busy` — in which case the settlement of
//!   that turn, whatever its outcome, is the restart. Loss and settlement therefore commute up
//!   to the due instant, which is the instant of whichever came second.
//! - *Totality*: every transition on a non-matching phase or unknown DID is the identity and
//!   reports `false`.

use std::collections::BTreeMap;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use rings_core::dht::Did;

/// Attempts in the rapid burst that follows a loss of reachability.
const BURST_ATTEMPTS: u8 = 5;
/// Delay between consecutive attempts inside the rapid burst.
const BURST_DELAY: Duration = Duration::from_secs(2);
/// Base cadence once the burst is exhausted, and the recheck period of a reachable target.
const BASE_INTERVAL: Duration = Duration::from_secs(300);
/// Inclusive upper bound of the uniform jitter added to every `BASE_INTERVAL` delay, so a fleet
/// that lost the same target at the same instant does not redial it in lockstep.
const JITTER_WINDOW: Duration = Duration::from_secs(30);

/// Lifecycle phase of one managed target; see the module diagram. The transitions are
/// functions of the phase and explicit time; the schedule only keys them by target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetPhase {
    /// Waiting for its next turn after `misses` consecutive turns that did not find the target
    /// reachable; due once `not_before_ms` has passed.
    Pending {
        /// Consecutive misses since the target was last reachable.
        misses: u8,
        /// Earliest instant of the next turn.
        not_before_ms: u64,
    },
    /// A turn is in flight; `lost` records a retirement of the target noticed meanwhile.
    Busy {
        /// Consecutive misses carried into this turn.
        misses: u8,
        /// Whether the target left the local DHT while the turn ran.
        lost: bool,
    },
    /// The last turn found the target reachable; reassessed once `recheck_at_ms` has passed.
    Reachable {
        /// Instant of the next periodic reassessment.
        recheck_at_ms: u64,
    },
}

impl TargetPhase {
    /// The phase every target starts in: due at once, with no misses.
    const INITIAL: Self = Self::Pending {
        misses: 0,
        not_before_ms: 0,
    };

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

    /// Enter `Busy` iff due at `now_ms`; `None` when busy or not yet due.
    fn begin_if_due(self, now_ms: u64) -> Option<Self> {
        if !self.is_due(now_ms) {
            return None;
        }
        let misses = match self {
            Self::Pending { misses, .. } => misses,
            Self::Reachable { .. } => 0,
            Self::Busy { .. } => return None,
        };
        Some(Self::Busy {
            misses,
            lost: false,
        })
    }

    /// Leave `Busy` with `outcome` at `now_ms`, drawing the next delay from `delays`; `None`
    /// when no turn is in flight.
    fn settle(self, outcome: TurnOutcome, now_ms: u64, delays: &mut DelaySampler) -> Option<Self> {
        let Self::Busy { misses, lost } = self else {
            return None;
        };
        Some(if lost {
            Self::Pending {
                misses: 0,
                not_before_ms: now_ms,
            }
        } else {
            match outcome {
                TurnOutcome::Reachable => Self::Reachable {
                    recheck_at_ms: now_ms.saturating_add(delays.slow_delay_ms()),
                },
                TurnOutcome::Unreachable => {
                    let misses = misses.saturating_add(1);
                    Self::Pending {
                        misses,
                        not_before_ms: now_ms.saturating_add(delays.delay_ms(misses)),
                    }
                }
            }
        })
    }

    /// Record at `now_ms` that the target left the local DHT; `None` when the running turn
    /// already noted a loss.
    fn notice_loss(self, now_ms: u64) -> Option<Self> {
        match self {
            Self::Reachable { .. } | Self::Pending { .. } => Some(Self::Pending {
                misses: 0,
                not_before_ms: now_ms,
            }),
            Self::Busy {
                misses,
                lost: false,
            } => Some(Self::Busy { misses, lost: true }),
            Self::Busy { lost: true, .. } => None,
        }
    }
}

/// Result of one supervisor turn, as reported by the shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TurnOutcome {
    /// The target was reachable through the overlay, or the redial completed with admission.
    Reachable,
    /// The turn did not find the target reachable: the redial failed, or was refused because
    /// the target's slot is owned by another attempt.
    Unreachable,
}

/// Samples the delay law: a rapid burst, then a jittered slow cadence drawn from a seeded
/// generator.
#[derive(Debug)]
struct DelaySampler(StdRng);

impl DelaySampler {
    /// Delay before the turn that follows the `misses`-th consecutive miss.
    fn delay_ms(&mut self, misses: u8) -> u64 {
        if misses < BURST_ATTEMPTS {
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

/// Retry schedule over every managed target: the phase machine keyed by target DID.
#[derive(Debug)]
pub(super) struct BootstrapSchedule {
    phases: BTreeMap<Did, TargetPhase>,
    delays: DelaySampler,
}

impl BootstrapSchedule {
    /// A schedule in which each of `targets` is due immediately, with jitter drawn from a
    /// generator seeded by `jitter_seed`.
    pub(super) fn new(targets: impl IntoIterator<Item = Did>, jitter_seed: u64) -> Self {
        Self {
            phases: targets
                .into_iter()
                .map(|target| (target, TargetPhase::INITIAL))
                .collect(),
            delays: DelaySampler(StdRng::seed_from_u64(jitter_seed)),
        }
    }

    /// Current phase of `target`, or `None` for a DID outside the target set.
    #[cfg(test)]
    fn phase(&self, target: Did) -> Option<TargetPhase> {
        self.phases.get(&target).copied()
    }

    /// Targets whose phase is due at `now_ms`, in DID order.
    #[cfg(test)]
    fn due(&self, now_ms: u64) -> impl Iterator<Item = Did> + '_ {
        self.phases
            .iter()
            .filter(move |(_, phase)| phase.is_due(now_ms))
            .map(|(target, _)| *target)
    }

    /// Apply `transition` to `target`'s phase; `false` when the DID is unknown or the transition
    /// is the identity on the current phase.
    fn transition(
        &mut self,
        target: Did,
        transition: impl FnOnce(TargetPhase, &mut DelaySampler) -> Option<TargetPhase>,
    ) -> bool {
        let Some(phase) = self.phases.get_mut(&target) else {
            return false;
        };
        match transition(*phase, &mut self.delays) {
            Some(next) => {
                *phase = next;
                true
            }
            None => false,
        }
    }

    /// Enter `Busy` for `target` iff it is due at `now_ms`; `false` when the target is unknown,
    /// busy, or not yet due.
    pub(super) fn begin_if_due(&mut self, target: Did, now_ms: u64) -> bool {
        self.transition(target, |phase, _| phase.begin_if_due(now_ms))
    }

    /// Leave `Busy` for `target` with `outcome` at `now_ms`; `false` when no turn was in flight.
    pub(super) fn settle(&mut self, target: Did, outcome: TurnOutcome, now_ms: u64) -> bool {
        self.transition(target, |phase, delays| {
            phase.settle(outcome, now_ms, delays)
        })
    }

    /// Record at `now_ms` that `target` left the local DHT; `false` when the running turn
    /// already noted a loss or the DID is unknown.
    pub(super) fn notice_loss(&mut self, target: Did, now_ms: u64) -> bool {
        self.transition(target, |phase, _| phase.notice_loss(now_ms))
    }

    /// Earliest instant at which some target becomes due, or `None` while every target is busy
    /// or the set is empty.
    pub(super) fn next_deadline_ms(&self) -> Option<u64> {
        self.phases
            .values()
            .copied()
            .filter_map(TargetPhase::deadline_ms)
            .min()
    }
}

/// Whole milliseconds of `duration`, saturating at `u64::MAX`.
pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Whether `instant_ms` lies exactly one slow-cadence delay after `from_ms`, that is within
/// `[from + BASE_INTERVAL, from + BASE_INTERVAL + JITTER_WINDOW]`.
#[cfg(test)]
pub(super) fn is_one_slow_delay_after(from_ms: u64, instant_ms: u64) -> bool {
    let floor = from_ms.saturating_add(duration_ms(BASE_INTERVAL));
    (floor..=floor.saturating_add(duration_ms(JITTER_WINDOW))).contains(&instant_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The timelines in this module's tests and the shell's spell the burst delay as `2_000`.
    const _: () = assert!(BURST_DELAY.as_millis() == 2_000);

    /// The single target of most tests.
    const TARGET: u32 = 1;

    /// A schedule over `count` targets with DIDs `1..=count`.
    fn schedule(count: u32) -> BootstrapSchedule {
        BootstrapSchedule::new((1..=count).map(Did::from), 1)
    }

    /// Run one missed turn for the target at `now_ms` and return the resulting `not_before_ms`.
    fn miss_turn(schedule: &mut BootstrapSchedule, now_ms: u64) -> u64 {
        let target = Did::from(TARGET);
        assert!(schedule.begin_if_due(target, now_ms));
        assert!(schedule.settle(target, TurnOutcome::Unreachable, now_ms));
        match schedule.phase(target) {
            Some(TargetPhase::Pending { not_before_ms, .. }) => not_before_ms,
            other => panic!("a missed turn must leave the target pending, got {other:?}"),
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

    /// Bounded-burst law: turns 1..BURST_ATTEMPTS wait BURST_DELAY, later ones one slow delay.
    #[test]
    fn burst_turns_are_two_seconds_apart_then_slow_down() {
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for turn in 1..BURST_ATTEMPTS {
            let next = miss_turn(&mut schedule, now_ms);
            assert_eq!(next, now_ms + 2_000, "turn {turn} must wait BURST_DELAY");
            now_ms = next;
        }
        let slow = miss_turn(&mut schedule, now_ms);
        assert!(is_one_slow_delay_after(now_ms, slow));
        let slower = miss_turn(&mut schedule, slow);
        assert!(is_one_slow_delay_after(slow, slower));
    }

    /// No-overlap law: a target that is not yet due cannot begin.
    #[test]
    fn a_target_cannot_begin_before_it_is_due() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        let next = miss_turn(&mut schedule, 0);
        assert!(!schedule.begin_if_due(target, next - 1));
        assert!(schedule.begin_if_due(target, next));
    }

    /// Reset-on-success law: a reachable settlement forgets misses and a loss restarts the burst.
    #[test]
    fn success_resets_the_miss_count() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for _ in 0..3 {
            now_ms = miss_turn(&mut schedule, now_ms);
        }
        assert!(schedule.begin_if_due(target, now_ms));
        assert!(schedule.settle(target, TurnOutcome::Reachable, now_ms));
        let Some(TargetPhase::Reachable { recheck_at_ms }) = schedule.phase(target) else {
            panic!("a reachable outcome must settle into Reachable");
        };
        assert!(is_one_slow_delay_after(now_ms, recheck_at_ms));
        assert!(schedule.notice_loss(target, recheck_at_ms - 1));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Pending {
                misses: 0,
                not_before_ms: recheck_at_ms - 1
            })
        );
        assert_eq!(
            miss_turn(&mut schedule, recheck_at_ms),
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
            now_ms = miss_turn(&mut schedule, now_ms);
        }
        assert!(matches!(
            schedule.phase(target),
            Some(TargetPhase::Pending {
                misses: BURST_ATTEMPTS,
                ..
            })
        ));
        assert!(schedule.notice_loss(target, 20));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Pending {
                misses: 0,
                not_before_ms: 20
            })
        );
    }

    /// A reachable target is due exactly at its recheck instant and begins with zero misses.
    #[test]
    fn a_reachable_target_is_rechecked_when_its_deadline_passes() {
        let target = Did::from(TARGET);
        let mut schedule = schedule(1);
        assert!(schedule.begin_if_due(target, 0));
        assert!(schedule.settle(target, TurnOutcome::Reachable, 0));
        let deadline = schedule
            .next_deadline_ms()
            .expect("a reachable target has a deadline");
        assert!(schedule.due(deadline - 1).next().is_none());
        assert_eq!(schedule.due(deadline).collect::<Vec<_>>(), vec![target]);
        assert!(schedule.begin_if_due(target, deadline));
        assert_eq!(
            schedule.phase(target),
            Some(TargetPhase::Busy {
                misses: 0,
                lost: false
            })
        );
    }

    /// Losses-are-never-lost law: a loss noticed while busy makes every settlement pending now
    /// with the burst restarted, whatever the outcome.
    #[test]
    fn a_loss_during_a_turn_restarts_the_burst_on_settlement() {
        let target = Did::from(TARGET);
        for outcome in [TurnOutcome::Reachable, TurnOutcome::Unreachable] {
            let mut schedule = schedule(1);
            let mut now_ms = 0;
            for _ in 0..BURST_ATTEMPTS {
                now_ms = miss_turn(&mut schedule, now_ms);
            }
            assert!(schedule.begin_if_due(target, now_ms));
            assert!(schedule.notice_loss(target, now_ms + 5));
            assert!(
                !schedule.notice_loss(target, now_ms + 6),
                "a second loss is already accounted for"
            );
            assert!(schedule.settle(target, outcome, now_ms + 10));
            assert_eq!(
                schedule.phase(target),
                Some(TargetPhase::Pending {
                    misses: 0,
                    not_before_ms: now_ms + 10
                }),
                "outcome {outcome:?} must not override the loss"
            );
        }
    }

    /// No-overlap law: a busy target cannot begin again, is never due, and has no deadline.
    #[test]
    fn a_busy_target_is_neither_due_nor_restartable() {
        let mut schedule = schedule(2);
        assert!(schedule.begin_if_due(Did::from(1), 0));
        assert!(!schedule.begin_if_due(Did::from(1), u64::MAX));
        assert_eq!(schedule.due(u64::MAX).collect::<Vec<_>>(), vec![Did::from(
            2
        )]);
        assert!(schedule.begin_if_due(Did::from(2), 0));
        assert_eq!(schedule.next_deadline_ms(), None);
    }

    /// Totality law: mismatched phases and unknown DIDs are identities reporting `false`.
    #[test]
    fn transitions_are_total_over_unknown_dids_and_phases() {
        let unknown = Did::from(7);
        let mut schedule = schedule(1);
        assert!(!schedule.begin_if_due(unknown, u64::MAX));
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
            let left_delay = left.delays.slow_delay_ms();
            let right_delay = right.delays.slow_delay_ms();
            assert_eq!(left_delay, right_delay);
            assert!(is_one_slow_delay_after(0, left_delay));
        }
    }

    /// The miss count saturates at `u8::MAX` instead of wrapping.
    #[test]
    fn miss_count_saturates() {
        let mut schedule = schedule(1);
        let mut now_ms = 0;
        for _ in 0..(u8::MAX as usize + 2) {
            now_ms = miss_turn(&mut schedule, now_ms);
        }
        assert!(matches!(
            schedule.phase(Did::from(TARGET)),
            Some(TargetPhase::Pending {
                misses: u8::MAX,
                ..
            })
        ));
    }
}
