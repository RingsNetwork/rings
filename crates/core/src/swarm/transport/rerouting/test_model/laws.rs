//! The propositions checked over the rerouting carrier, and the carrier as an instance of the
//! shared search.
//!
//! Safety (`□`): `EffectsFollowOutcome` (S1: it implies `effects ≤ 1`), `FreshHop` (S2),
//! `SendsBounded` (S3), `ExhaustionCarriesLastCause` (S3), `VerdictsClassified`. Coverage
//! (`◇`), so the safety laws are not vacuous: `RetryReachesReplacement`,
//! `WaitEndsByRouteChange`, `WaitEndsByCapacityRelease`, `WaitEndsByChannelDrain`,
//! `AmbiguityEndsUnretried`, `BudgetExhausts`.
//!
//! Liveness (L1) is decided by the search over [`is_accepted`] under [`has_liveness_budget`].

use std::fmt;

use super::super::QUIESCENT_DEFERRALS;
use super::super::REROUTING_BUDGET;
use super::carrier::Action;
use super::carrier::Model;
use super::carrier::Outcome;
use super::carrier::Phase;
use super::carrier::State;
use crate::swarm::transport::test_model_check::CheckedModel;
use crate::swarm::transport::test_model_check::Expectation;
use crate::swarm::transport::test_model_check::Law;

/// The identity of a law, as verdicts and mutation tests refer to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LawName {
    /// `□` (S1): `effects = 1` exactly when the placement ended accepted or ambiguous, and
    /// `effects = 0` while it is in progress or after exhaustion; so `effects ≤ 1`.
    EffectsFollowOutcome,
    /// `□ ¬stale_retry` (S2): every wake satisfies the model's own freshness condition.
    FreshHop,
    /// `□ sends ≤ REROUTING_BUDGET + 1` (S3).
    SendsBounded,
    /// `□` (S3): exhaustion reports `REROUTING_BUDGET + 1` deferrals and the last cause.
    ExhaustionCarriesLastCause,
    /// `□`: every verdict projects onto an expected outcome (no `Unexpected`).
    VerdictsClassified,
    /// `◇`: a retry is accepted by a generation newer than the first.
    RetryReachesReplacement,
    /// `◇`: a wait ends because the route moved.
    WaitEndsByRouteChange,
    /// `◇`: a wait ends because capacity was released.
    WaitEndsByCapacityRelease,
    /// `◇`: a wait ends because the hop's channel drained or its generation changed.
    WaitEndsByChannelDrain,
    /// `◇`: an ambiguous failure ends the placement.
    AmbiguityEndsUnretried,
    /// `◇`: the budget is exhausted.
    BudgetExhausts,
}

impl fmt::Display for LawName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

/// S1: the effect count is the one the phase implies.
fn effects_follow_outcome(_: &Model, state: &State) -> bool {
    let expected = match state.phase {
        Phase::Done(Outcome::Accepted | Outcome::Ambiguous) => 1,
        Phase::Compute { .. } | Phase::InFlight { .. } | Phase::Waiting { .. } => 0,
        Phase::Done(Outcome::Exhausted { .. } | Outcome::Unexpected) => 0,
    };
    state.effects == expected
}

/// S2: no stale retry.
fn fresh_hop(_: &Model, state: &State) -> bool {
    !state.stale_retry
}

/// S3: bounded sends.
fn sends_bounded(_: &Model, state: &State) -> bool {
    state.sends <= REROUTING_BUDGET + 1
}

/// S3: a typed exhaustion carrying the last cause.
fn exhaustion_carries_last_cause(_: &Model, state: &State) -> bool {
    match state.phase {
        Phase::Done(Outcome::Exhausted {
            deferrals,
            last_matches,
        }) => deferrals == REROUTING_BUDGET + 1 && last_matches,
        Phase::Compute { .. } | Phase::InFlight { .. } | Phase::Waiting { .. } => true,
        Phase::Done(Outcome::Accepted | Outcome::Ambiguous | Outcome::Unexpected) => true,
    }
}

/// Every verdict projects onto an expected outcome.
fn verdicts_classified(_: &Model, state: &State) -> bool {
    state.phase != Phase::Done(Outcome::Unexpected)
}

/// Coverage: a retry reached a replacement generation.
fn retry_reaches_replacement(_: &Model, state: &State) -> bool {
    state.reached_replacement
}

/// Coverage: a wait ended by a route change.
fn wait_ends_by_route_change(_: &Model, state: &State) -> bool {
    state.woke_on_route
}

/// Coverage: a wait ended by a capacity release.
fn wait_ends_by_capacity_release(_: &Model, state: &State) -> bool {
    state.woke_on_capacity
}

/// Coverage: a wait ended by a channel drain or a generation change.
fn wait_ends_by_channel_drain(_: &Model, state: &State) -> bool {
    state.woke_on_drain
}

/// Coverage: an ambiguous failure ended the placement.
fn ambiguity_ends_unretried(_: &Model, state: &State) -> bool {
    state.phase == Phase::Done(Outcome::Ambiguous)
}

/// Coverage: the budget was exhausted.
fn budget_exhausts(_: &Model, state: &State) -> bool {
    matches!(state.phase, Phase::Done(Outcome::Exhausted { .. }))
}

/// `Converged(s)`: the placement was accepted (L1's target).
pub(super) fn is_accepted(state: &State) -> bool {
    state.phase == Phase::Done(Outcome::Accepted)
}

/// L1's premise at the state where churn stops: the placement is in progress with at least
/// `QUIESCENT_DEFERRALS` deferrals of budget left, whatever the channels' occupancy (checked up
/// to three foreign transfers on one channel; the general claim is the occupancy argument of
/// `rerouting`'s L1).
pub(super) fn has_liveness_budget(state: &State) -> bool {
    let deferrals = match state.phase {
        Phase::Compute { deferrals }
        | Phase::InFlight { deferrals, .. }
        | Phase::Waiting { deferrals, .. } => deferrals,
        Phase::Done(_) => return false,
    };
    deferrals <= REROUTING_BUDGET - QUIESCENT_DEFERRALS
}

/// Every checked proposition, in report order.
pub(super) const LAWS: [Law<Model>; 11] = [
    Law {
        name: LawName::EffectsFollowOutcome,
        expectation: Expectation::Always,
        holds: effects_follow_outcome,
    },
    Law {
        name: LawName::FreshHop,
        expectation: Expectation::Always,
        holds: fresh_hop,
    },
    Law {
        name: LawName::SendsBounded,
        expectation: Expectation::Always,
        holds: sends_bounded,
    },
    Law {
        name: LawName::ExhaustionCarriesLastCause,
        expectation: Expectation::Always,
        holds: exhaustion_carries_last_cause,
    },
    Law {
        name: LawName::VerdictsClassified,
        expectation: Expectation::Always,
        holds: verdicts_classified,
    },
    Law {
        name: LawName::RetryReachesReplacement,
        expectation: Expectation::Sometimes,
        holds: retry_reaches_replacement,
    },
    Law {
        name: LawName::WaitEndsByRouteChange,
        expectation: Expectation::Sometimes,
        holds: wait_ends_by_route_change,
    },
    Law {
        name: LawName::WaitEndsByCapacityRelease,
        expectation: Expectation::Sometimes,
        holds: wait_ends_by_capacity_release,
    },
    Law {
        name: LawName::WaitEndsByChannelDrain,
        expectation: Expectation::Sometimes,
        holds: wait_ends_by_channel_drain,
    },
    Law {
        name: LawName::AmbiguityEndsUnretried,
        expectation: Expectation::Sometimes,
        holds: ambiguity_ends_unretried,
    },
    Law {
        name: LawName::BudgetExhausts,
        expectation: Expectation::Sometimes,
        holds: budget_exhausts,
    },
];

/// The rerouting carrier as an instance of the shared search.
impl CheckedModel for Model {
    type State = State;
    type Action = Action;
    type LawName = LawName;

    fn init(&self) -> State {
        Model::init(self)
    }

    fn actions(&self, state: &State) -> Vec<Action> {
        Model::actions(self, state)
    }

    fn next_state(&self, state: &State, action: &Action) -> Option<State> {
        Model::next_state(self, state, action)
    }

    fn laws(&self) -> &[Law<Self>] {
        &LAWS
    }

    fn is_converged(&self, state: &State) -> bool {
        is_accepted(state)
    }

    fn is_environmental(action: &Action) -> bool {
        action.is_environmental()
    }

    fn is_periodic(_: &Action) -> bool {
        false
    }
}
