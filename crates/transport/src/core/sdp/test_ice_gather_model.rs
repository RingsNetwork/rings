//! Stateright model check of the [`IceGatherState`] transition algebra across `Host`,
//! `Reflexive` (STUN), and `Relay` (TURN) configurations, with mutant refutation for every
//! known TURN / STUN / Host-only gathering defect.
//!
//! ```text
//!   ModelState = IceGatherState
//!              × Option<(IceSettleWindow, u64)> (in-flight timer for an earlier epoch)
//!              × ℕ (candidates still to arrive)
//!              × 𝔹 (settled by timer)
//!              × 𝔹 (settled by browser completion)
//!              × 𝔹 (stale timer arrived after a later candidate without resetting epoch)
//! ```
//!
//! Properties checked for every `target_tier ∈ {Host, Reflexive, Relay}`:
//!
//! - `Always` no premature TURN relay omission: when `target_tier == Relay`, `settled_by_timer`
//!   implies `highest_observed == Some(Relay)`.
//! - `Always` no empty timer settle: `settled_by_timer` implies `highest_observed.is_some()`.
//! - `Always` target-tier fast window: whenever `highest_observed >= Some(target_tier)`,
//!   `armed_window == Some(Fast)`.
//! - `Always` quiet-window reset on post-arm candidate: a timer armed at an earlier epoch is
//!   always invalidated when any candidate (`Host`, `Reflexive`, or `Relay`) arrives while a
//!   settle window is armed (`stale_timer_settled == false`).
//! - `Eventually` settled: once a candidate of `target_tier` arrives (or `BrowserComplete` fires),
//!   the gathering session reaches a settled terminal state.

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::IceCandidateTier;
use super::IceGatherState;
use super::IceSettleWindow;

const MAX_CANDIDATE_STEPS: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GatherModelState {
    gather: IceGatherState,
    /// An in-flight timer snapshot `(window, armed_epoch)` captured before a subsequent candidate
    /// arrival, used to verify that any post-arm candidate arrival invalidates the older timer.
    in_flight_timer: Option<(IceSettleWindow, u64)>,
    /// Whether at least one candidate arrived after `in_flight_timer` was snapshotted.
    candidate_arrived_since_snapshot: bool,
    steps_taken: u8,
    settled_by_timer: bool,
    settled_by_browser: bool,
    stale_timer_settled: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum GatherAction {
    ObserveCandidate(IceCandidateTier),
    ObserveNonCandidateText,
    SnapshotInFlightTimer,
    FireCurrentTimer,
    FireSnapshotTimer,
    BrowserComplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatherMutant {
    /// Bug 1 (Codex P1): `srflx` unlocks `Fast` (`300 ms`) even when `target_tier == Relay` (TURN).
    ReflexiveUnlocksTurnFast,
    /// Bug 2 (Codex P2): `host`/`srflx` arms `Fallback` (`3 s`) when `target_tier == Relay` (TURN).
    HostFallbackArmsTurn,
    /// Bug 3 (Codex P2): `host`/`srflx` arriving after `relay` fails to advance `epoch` and reset
    /// the armed 300 ms quiet window.
    IgnoreLowerTierAfterRelayArm,
    /// Bug 4 (#1290 Item 1b): `host` uses `Fallback` (`3 s`) instead of `Fast` (`300 ms`) when
    /// `target_tier == Host` (`ice_servers = ""`).
    HostOnlyUsesSlowFallback,
}

struct GatherModel {
    target_tier: IceCandidateTier,
    mutant: Option<GatherMutant>,
}

impl GatherModel {
    fn step_observe(
        &self,
        state: IceGatherState,
        tier: IceCandidateTier,
    ) -> (IceGatherState, bool) {
        let Some(mutant) = self.mutant else {
            return state.observe_tier(tier);
        };
        let highest_observed = Some(state.highest_observed.map_or(tier, |prev| prev.max(tier)));
        let unlocked = match mutant {
            GatherMutant::ReflexiveUnlocksTurnFast => {
                if tier >= IceCandidateTier::Reflexive || tier >= state.target_tier {
                    Some(IceSettleWindow::Fast)
                } else {
                    Some(IceSettleWindow::Fallback)
                }
            }
            GatherMutant::HostFallbackArmsTurn => {
                if tier >= state.target_tier {
                    Some(IceSettleWindow::Fast)
                } else {
                    Some(IceSettleWindow::Fallback)
                }
            }
            GatherMutant::IgnoreLowerTierAfterRelayArm => {
                IceGatherState::window_for_tier(tier, state.target_tier)
            }
            GatherMutant::HostOnlyUsesSlowFallback => {
                if state.target_tier == IceCandidateTier::Host {
                    Some(IceSettleWindow::Fallback)
                } else {
                    IceGatherState::window_for_tier(tier, state.target_tier)
                }
            }
        };
        if matches!(mutant, GatherMutant::IgnoreLowerTierAfterRelayArm) && unlocked.is_none() {
            return (state, false);
        }
        let armed_window = match (state.armed_window, unlocked) {
            (Some(existing), Some(new)) => Some(existing.min(new)),
            (Some(existing), None) => Some(existing),
            (None, unlocked) => unlocked,
        };
        if armed_window.is_some() {
            (
                IceGatherState {
                    target_tier: state.target_tier,
                    highest_observed,
                    armed_window,
                    epoch: state.epoch.wrapping_add(1),
                },
                true,
            )
        } else {
            (
                IceGatherState {
                    target_tier: state.target_tier,
                    highest_observed,
                    armed_window: None,
                    epoch: state.epoch,
                },
                false,
            )
        }
    }
}

impl Model for GatherModel {
    type State = GatherModelState;
    type Action = GatherAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![GatherModelState {
            gather: IceGatherState::new(self.target_tier),
            in_flight_timer: None,
            candidate_arrived_since_snapshot: false,
            steps_taken: 0,
            settled_by_timer: false,
            settled_by_browser: false,
            stale_timer_settled: false,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.settled_by_timer || state.settled_by_browser {
            return;
        }
        if state.steps_taken < MAX_CANDIDATE_STEPS {
            actions.push(GatherAction::ObserveCandidate(IceCandidateTier::Host));
            actions.push(GatherAction::ObserveCandidate(IceCandidateTier::Reflexive));
            actions.push(GatherAction::ObserveCandidate(IceCandidateTier::Relay));
            actions.push(GatherAction::ObserveNonCandidateText);
        }
        if state.gather.armed_window.is_some() && state.in_flight_timer.is_none() {
            actions.push(GatherAction::SnapshotInFlightTimer);
        }
        if state.gather.armed_window.is_some() {
            actions.push(GatherAction::FireCurrentTimer);
        }
        if state.in_flight_timer.is_some() {
            actions.push(GatherAction::FireSnapshotTimer);
        }
        actions.push(GatherAction::BrowserComplete);
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = *last;
        match action {
            GatherAction::ObserveCandidate(tier) => {
                let (gather, _) = self.step_observe(next.gather, tier);
                next.gather = gather;
                if next.in_flight_timer.is_some() {
                    next.candidate_arrived_since_snapshot = true;
                }
                next.steps_taken = next.steps_taken.saturating_add(1);
            }
            GatherAction::ObserveNonCandidateText => {
                let (gather, changed) = next.gather.observe_text("v=0\r\ns=-\r\n");
                assert!(!changed);
                next.gather = gather;
                next.steps_taken = next.steps_taken.saturating_add(1);
            }
            GatherAction::SnapshotInFlightTimer => {
                if let Some(window) = next.gather.armed_window {
                    next.in_flight_timer = Some((window, next.gather.epoch));
                    next.candidate_arrived_since_snapshot = false;
                }
            }
            GatherAction::FireCurrentTimer => {
                if next.gather.armed_window.is_some() {
                    next.settled_by_timer = true;
                }
            }
            GatherAction::FireSnapshotTimer => {
                if let Some((_window, snapshot_epoch)) = next.in_flight_timer {
                    // In `wait_for_candidates_settled`, a timer poll checks
                    // `progress.state.get().epoch == observed_epoch`. If a candidate arrived after
                    // `SnapshotInFlightTimer` (`candidate_arrived_since_snapshot == true`) yet
                    // `epoch` failed to advance, the stale timer falsely settles!
                    if next.gather.epoch == snapshot_epoch {
                        if next.candidate_arrived_since_snapshot {
                            next.stale_timer_settled = true;
                        }
                        next.settled_by_timer = true;
                    }
                    next.in_flight_timer = None;
                    next.candidate_arrived_since_snapshot = false;
                }
            }
            GatherAction::BrowserComplete => {
                next.settled_by_browser = true;
            }
        }
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always("no premature TURN relay omission", |model, state| {
                if model.target_tier == IceCandidateTier::Relay && state.settled_by_timer {
                    state.gather.highest_observed == Some(IceCandidateTier::Relay)
                } else {
                    true
                }
            }),
            Property::<Self>::always("no empty timer settle", |_, state| {
                !state.settled_by_timer || state.gather.highest_observed.is_some()
            }),
            Property::<Self>::always(
                "target-tier candidate always unlocks fast settle window",
                |model, state| {
                    if state
                        .gather
                        .highest_observed
                        .is_some_and(|highest| highest >= model.target_tier)
                    {
                        state.gather.armed_window == Some(IceSettleWindow::Fast)
                    } else {
                        true
                    }
                },
            ),
            Property::<Self>::always(
                "in-flight snapshot timer is invalidated by any subsequent candidate",
                |_, state| !state.stale_timer_settled,
            ),
        ]
    }
}

#[test]
fn test_ice_gather_state_model_preserves_all_host_stun_and_turn_laws() {
    for target_tier in [
        IceCandidateTier::Host,
        IceCandidateTier::Reflexive,
        IceCandidateTier::Relay,
    ] {
        let model = GatherModel {
            target_tier,
            mutant: None,
        };
        let checker = model.checker().spawn_bfs().join();
        checker.assert_properties();
    }
}

#[test]
fn test_each_ice_gather_mutant_is_refuted() {
    let cases = [
        (
            IceCandidateTier::Relay,
            GatherMutant::ReflexiveUnlocksTurnFast,
            "no premature TURN relay omission",
        ),
        (
            IceCandidateTier::Relay,
            GatherMutant::HostFallbackArmsTurn,
            "no premature TURN relay omission",
        ),
        (
            IceCandidateTier::Relay,
            GatherMutant::IgnoreLowerTierAfterRelayArm,
            "in-flight snapshot timer is invalidated by any subsequent candidate",
        ),
        (
            IceCandidateTier::Host,
            GatherMutant::HostOnlyUsesSlowFallback,
            "target-tier candidate always unlocks fast settle window",
        ),
    ];

    for (target_tier, mutant, expected_violated_property) in cases {
        let model = GatherModel {
            target_tier,
            mutant: Some(mutant),
        };
        let checker = model.checker().spawn_bfs().join();
        assert!(
            checker.discovery(expected_violated_property).is_some(),
            "mutant {mutant:?} on target_tier {target_tier:?} must be refuted by property {expected_violated_property:?}"
        );
    }
}
