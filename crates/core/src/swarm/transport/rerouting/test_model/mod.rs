//! Exhaustive model check of the rerouting automaton (#859), on the search shared with the
//! Chord rejoin model (`test_model_check`, #772).
//!
//! # Specification (TLA+ style)
//!
//! ```text
//! CONSTANTS  B = REROUTING_BUDGET, Churn (reservations, glare, withdrawals, deaths,
//!            disconnects, reroutes, congestions, jams, fills, queues, ambiguities)
//!
//! VARIABLES  registry   : ConnectionLifecycleRegistry of the target hop   (production)
//!            ready      : BOOLEAN                \* the admitted generation can make progress
//!            preference : {Target, Alternate, Local}
//!            congested  : BOOLEAN                \* shared (global) capacity exhausted
//!            jam        : Hop → ℕ                \* other transfers holding each channel
//!            full       : Hop → BOOLEAN          \* a hop's own capacity exhausted
//!            queued     : BOOLEAN                \* a waiter queued on the shared capacity
//!            drained    : Hop → ℕ                \* each hop's transfers ended on its link
//!            phase      : Compute(d) | InFlight(d, hop, g)
//!                       | Waiting(d, hop, g, (drained₀, ahead), cause) | Done(outcome)
//!            effects, sends, last_refusal, stale_retry : history variables
//!
//! Init ≜ registry = {Target ↦ Active(g₁)} ∧ ready ∧ preference = Target ∧ phase = Compute(0)
//! Next ≜ Env ∨ Protocol                                                     (see `next`)
//! Route ≜ Local if preference = Local
//!         Target if preference = Target ∧ Admitted(Target)       \* TopologyReferencesOnlyAdmitted
//!         Alternate otherwise
//!
//! Safety (□):  effects ≤ 1                                        (S1)
//!              effects = [phase ∈ Done(Accepted | Ambiguous)]      (S1)
//!              ¬stale_retry                                        (S2)
//!              sends ≤ B + 1 ∧ Exhausted ⇒ deferrals = B + 1 ∧ last cause   (S3)
//! Liveness:    Fairness ≜ WF(Admit) ∧ WF(Recover) ∧ WF(Close) ∧ WF(Release) ∧ WF(Drain(h))
//!                         ∧ WF(Dequeue)
//!                         ∧ WF(Send) ∧ WF(Accept) ∧ WF(Refuse(r)) ∧ WF(Wake)
//!              ∀r. Premise(r) ⇒ (□[Protocol] ∧ Fairness ⇒ ◇ Done(Accepted))      (L1)
//!              Premise(r) ≜ phase ∉ Done ∧ deferrals ≤ B − QUIESCENT_DEFERRALS
//!                           ∧ TopologyReferencesOnlyAdmitted   \* built into Route below
//! ```
//!
//! `WF(Admit)` is the weak fairness of readiness: a registered generation of the target
//! eventually opens its data channel and is admitted, or the environment withdraws it.
//!
//! # Fidelity
//!
//! - Every generation transition is a method of the production registry (`reserve`,
//!   `begin_admission`/`activate`, `remove_unadmitted`, `mark_send_terminal`,
//!   `retire_active_if`); generations are the registry's own.
//! - Every transition of the automaton is the production `Rerouting::after` on a production
//!   `Verdict`, built by the production `Verdict::remote` from production errors; every wake
//!   guard is the production `Awaiting::is_triggered`. A classification or trigger that
//!   regresses changes the explored graph.
//! - What the model owns is the environment and the resolution relation of the send path
//!   (`next::resolutions`), which states the send-path lemmas of `error::send_class`: refusals
//!   occur only before acceptance, and ambiguities only after it.
//! - The refused transfer's own capacity release is not an event of the model, as in
//!   production no trigger reads it: `Room` is a state, decided by the production combinator
//!   `admits_now` over the model's scopes (`State::has_room`: the hop's own capacity, the
//!   shared capacity, and whether a waiter is queued on it), and the channel mark
//!   `(drained₀, ahead)` is read after the refusal, as `PeerStamp` is.
//! - The combinator is shared, but its scopes are abstract booleans: the fixed reservation is
//!   pinned to `false` (every modeled demand exceeds it) and no waiter queues on a hop's own
//!   capacity. So the branch where a small demand bypasses a non-empty queue through its fixed
//!   reservation is not explored; it only admits sooner than the modeled shared branch.
//! - Releases are scoped as in production: a hop's `Drain` and `Close` free its own capacity
//!   and count its ended transfers without clearing shared congestion; only `Release` does.
//!
//! # Scope limits
//!
//! - One placement; the placements of one operation are independent (distinct messages).
//! - The alternate hop is stable, and a route through the target needs an admitted
//!   generation; routing churn is the preference change.
//! - The listeners of the shell (topology, link and capacity epochs) are not modeled: the wake
//!   guard is a state predicate, so a wait registered before its check misses no event
//!   (`Law (Wake)` of `lifecycle::epoch`), and `Wake` is enabled exactly when the guard holds.
//!
//! # Bounds
//!
//! Each search is exhaustive with no depth cut-off; generations ≤ 3 (two reservations after
//! the first). Each test asserts its exact state count, depth and premise counts.
//!
//! Churn columns: reservations `r`, glare `g`, withdrawals `w`, deaths `d`, disconnects `x`,
//! reroutes `t`, congestions `c`, jams `j`, fills `f`, queues `q`, ambiguities `a`.
//!
//! | configuration       | r g w d x t c j f q a | states | depth | premise (unsettled) |
//! |---------------------|-----------------------|--------|-------|---------------------|
//! | replacement         | 2 0 0 2 1 0 0 0 0 0 1 | 1731   | 21    | 733 (733)           |
//! | glare               | 2 1 0 1 0 0 1 0 0 0 0 | 710    | 18    | 424 (424)           |
//! | retire before ready | 2 0 1 2 0 0 0 0 0 0 0 | 631    | 16    | 408 (408)           |
//! | topology mid-wait   | 1 0 0 1 1 2 1 1 0 0 0 | 130776 | 29    | 38532 (38532)       |
//! | exhaustion          | 2 0 0 2 2 0 2 1 0 0 0 | 257719 | 35    | 44140 (44140)       |
//! | channel drain       | 1 0 0 1 0 0 0 2 0 0 0 | 2900   | 19    | 1650 (1650)         |
//! | deep channel drain  | 1 0 0 1 0 0 0 3 0 0 0 | 9067   | 24    | 4599 (4599)         |
//! | scoped capacity     | 1 0 0 1 0 0 1 2 2 1 0 | 185372 | 31    | 61026 (61026)       |
//! | one replacement     | 1 0 0 1 0 0 0 0 0 0 0 | max deferrals = `REPLACEMENT_DEFERRALS`       |

mod carrier;
mod laws;
mod next;

use std::collections::HashSet;

use carrier::Churn;
use carrier::Model;
use carrier::Mutation;
use laws::has_liveness_budget;
use laws::LawName;

use super::QUIESCENT_DEFERRALS;
use super::REPLACEMENT_DEFERRALS;
use super::REROUTING_BUDGET;
use crate::swarm::transport::test_model_check::check;
use crate::swarm::transport::test_model_check::LivenessAnalysis;
use crate::swarm::transport::test_model_check::SearchReport;

/// The exact figures one configuration's search must reproduce.
#[derive(Debug, PartialEq, Eq)]
struct Bounds {
    /// `|G|`.
    states: usize,
    /// Longest breadth-first level.
    depth: usize,
    /// Premise states.
    premise_states: usize,
    /// Premise states outside `Stable`.
    unstable_premise_states: usize,
}

/// No churn: the base every configuration adds to.
const QUIET: Churn = Churn {
    reservations: 0,
    glare: 0,
    withdrawals: 0,
    deaths: 0,
    disconnects: 0,
    reroutes: 0,
    congestions: 0,
    jams: 0,
    fills: 0,
    queues: 0,
    ambiguities: 0,
};

/// The topology-mid-wait churn: the route moves while a placement waits.
const TOPOLOGY_MID_WAIT: Churn = Churn {
    reservations: 1,
    deaths: 1,
    reroutes: 2,
    congestions: 1,
    jams: 1,
    disconnects: 1,
    ..QUIET
};

/// The scoped-capacity churn: per-peer fills beside shared congestion.
const SCOPED_CAPACITY: Churn = Churn {
    reservations: 1,
    deaths: 1,
    congestions: 1,
    jams: 2,
    fills: 2,
    queues: 1,
    ..QUIET
};

/// Search `churn` under the production automaton and assert every law, the exact bounds,
/// coverage of `covered`, and L1.
fn assert_laws_hold_exhaustively(name: &str, churn: Churn, bounds: Bounds, covered: &[LawName]) {
    let model = Model {
        churn,
        mutation: Mutation::Faithful,
    };
    let report = check(&model, has_liveness_budget);
    let SearchReport::Safe {
        states,
        max_depth,
        uncovered,
        liveness:
            LivenessAnalysis {
                premise_states,
                unstable_premise_states,
                stable_states,
                violation,
            },
    } = report
    else {
        panic!("{name}: {report:#?}");
    };
    println!(
        "{name}: {states} states, depth {max_depth}, premise {premise_states} \
         ({unstable_premise_states} unsettled), stable {stable_states}"
    );
    assert!(violation.is_none(), "{name}: {violation:#?}");
    for law in covered {
        assert!(!uncovered.contains(law), "{name}: {law} uncovered");
    }
    assert_eq!(
        Bounds {
            states,
            depth: max_depth,
            premise_states,
            unstable_premise_states,
        },
        bounds,
        "{name}: stale bounds table"
    );
    assert!(unstable_premise_states > 0, "{name}: vacuous premise");
    assert!(stable_states > 0, "{name}: unreachable target");
}

/// Search `churn` under `mutation` and require a violation of `law`.
fn assert_mutation_violates(churn: Churn, mutation: Mutation, law: LawName) {
    let report = check(&Model { churn, mutation }, has_liveness_budget);
    let SearchReport::Unsafe { violation, .. } = report else {
        panic!("{mutation:?} must violate {law}: {report:#?}");
    };
    assert_eq!(violation.law, law, "{violation:#?}");
}

/// Generation replacement: deaths and re-admissions of the target racing the placement.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_across_generation_replacement() {
    assert_laws_hold_exhaustively(
        "replacement",
        Churn {
            reservations: 2,
            deaths: 2,
            disconnects: 1,
            ambiguities: 1,
            ..QUIET
        },
        Bounds {
            states: 1731,
            depth: 21,
            premise_states: 733,
            unstable_premise_states: 733,
        },
        &[
            LawName::RetryReachesReplacement,
            LawName::WaitEndsByRouteChange,
            LawName::AmbiguityEndsUnretried,
        ],
    );
}

/// Glare: the pending replacement is withdrawn for the peer's offer.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_under_glare() {
    assert_laws_hold_exhaustively(
        "glare",
        Churn {
            reservations: 2,
            glare: 1,
            deaths: 1,
            congestions: 1,
            ..QUIET
        },
        Bounds {
            states: 710,
            depth: 18,
            premise_states: 424,
            unstable_premise_states: 424,
        },
        &[
            LawName::RetryReachesReplacement,
            LawName::WaitEndsByCapacityRelease,
        ],
    );
}

/// Retire-before-ready: a replacement is withdrawn before its data channel opens.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_when_retired_before_ready() {
    assert_laws_hold_exhaustively(
        "retire before ready",
        Churn {
            reservations: 2,
            withdrawals: 1,
            deaths: 2,
            ..QUIET
        },
        Bounds {
            states: 631,
            depth: 16,
            premise_states: 408,
            unstable_premise_states: 408,
        },
        &[
            LawName::RetryReachesReplacement,
            LawName::WaitEndsByRouteChange,
        ],
    );
}

/// Topology change mid-wait: the route moves (to the alternate, or here) while waiting.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_when_topology_changes_mid_wait() {
    assert_laws_hold_exhaustively(
        "topology mid-wait",
        TOPOLOGY_MID_WAIT,
        Bounds {
            states: 130776,
            depth: 29,
            premise_states: 38532,
            unstable_premise_states: 38532,
        },
        &[
            LawName::WaitEndsByRouteChange,
            LawName::WaitEndsByCapacityRelease,
        ],
    );
}

/// Channel drain: other transfers jam the hops' channels; a queue timeout waits for their
/// drain (never for its own release) or a generation change.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_when_channels_drain() {
    assert_laws_hold_exhaustively(
        "channel drain",
        Churn {
            reservations: 1,
            deaths: 1,
            jams: 2,
            ..QUIET
        },
        Bounds {
            states: 2900,
            depth: 19,
            premise_states: 1650,
            unstable_premise_states: 1650,
        },
        &[
            LawName::WaitEndsByChannelDrain,
            LawName::RetryReachesReplacement,
        ],
    );
}

/// Deep channel drain: up to three foreign transfers occupy one channel when the environment
/// stops. L1 holds with the same `QUIESCENT_DEFERRALS`, so the bound does not grow with the
/// residual occupancy (the drained-ahead stamp; the general claim is its argument in
/// `rerouting`'s L1, of which this configuration is the check one step past `jams = 2`).
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_when_a_channel_is_deeply_occupied() {
    assert_laws_hold_exhaustively(
        "deep channel drain",
        Churn {
            reservations: 1,
            deaths: 1,
            jams: 3,
            ..QUIET
        },
        Bounds {
            states: 9067,
            depth: 24,
            premise_states: 4599,
            unstable_premise_states: 4599,
        },
        &[
            LawName::WaitEndsByChannelDrain,
            LawName::RetryReachesReplacement,
        ],
    );
}

/// Scoped capacity: a hop's own capacity fills beside shared congestion; the hop's releases
/// free its own scope but not the shared one, and a capacity wait ends only on `Room`.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_laws_hold_under_scoped_capacity() {
    assert_laws_hold_exhaustively(
        "scoped capacity",
        SCOPED_CAPACITY,
        Bounds {
            states: 185372,
            depth: 31,
            premise_states: 61026,
            unstable_premise_states: 61026,
        },
        &[
            LawName::WaitEndsByCapacityRelease,
            LawName::WaitEndsByChannelDrain,
        ],
    );
}

/// `REPLACEMENT_DEFERRALS` is tight: under one generation replacement racing the placement
/// (a death, then a re-admission) and nothing else, every reachable state has spent at most
/// `REPLACEMENT_DEFERRALS` deferrals, and some state has spent exactly that many.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_one_replacement_spends_exactly_the_replacement_deferrals() {
    let model = Model {
        churn: Churn {
            reservations: 1,
            deaths: 1,
            ..QUIET
        },
        mutation: Mutation::Faithful,
    };
    let mut seen = HashSet::new();
    let mut frontier = vec![model.init()];
    let mut most = 0;
    while let Some(state) = frontier.pop() {
        most = most.max(spent(&state.phase));
        for action in model.actions(&state) {
            if let Some(next) = model.next_state(&state, &action) {
                if seen.insert(next.clone()) {
                    frontier.push(next);
                }
            }
        }
    }
    assert_eq!(most, REPLACEMENT_DEFERRALS);
}

/// The deferrals a phase has spent, the exhausting one included.
fn spent(phase: &carrier::Phase) -> u8 {
    match *phase {
        carrier::Phase::Compute { deferrals }
        | carrier::Phase::InFlight { deferrals, .. }
        | carrier::Phase::Waiting { deferrals, .. }
        | carrier::Phase::Done(carrier::Outcome::Exhausted { deferrals, .. }) => deferrals,
        carrier::Phase::Done(
            carrier::Outcome::Accepted | carrier::Outcome::Ambiguous | carrier::Outcome::Unexpected,
        ) => 0,
    }
}

/// Exhaustion: churn enough to spend the budget, which ends typed with the last cause.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_rerouting_budget_exhausts_with_the_last_cause() {
    assert_laws_hold_exhaustively(
        "exhaustion",
        Churn {
            reservations: 2,
            deaths: 2,
            disconnects: 2,
            congestions: 2,
            jams: 1,
            ..QUIET
        },
        Bounds {
            states: 257719,
            depth: 35,
            premise_states: 44140,
            unstable_premise_states: 44140,
        },
        &[LawName::BudgetExhausts],
    );
}

/// L1 is tight: one deferral less of budget at quiescence admits a fair trace that exhausts
/// (the route moves to a replacement admitted after churn stopped, under exhausted capacity).
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_liveness_needs_the_quiescent_deferrals() {
    /// The premise with one deferral less of budget than `has_liveness_budget`.
    fn one_deferral_short(state: &carrier::State) -> bool {
        match state.phase {
            carrier::Phase::Compute { deferrals }
            | carrier::Phase::InFlight { deferrals, .. }
            | carrier::Phase::Waiting { deferrals, .. } => {
                deferrals == REROUTING_BUDGET - QUIESCENT_DEFERRALS + 1
            }
            carrier::Phase::Done(_) => false,
        }
    }
    let model = Model {
        churn: TOPOLOGY_MID_WAIT,
        mutation: Mutation::Faithful,
    };
    let SearchReport::Safe { liveness, .. } = check(&model, one_deferral_short) else {
        panic!("the faithful automaton is safe");
    };
    assert!(liveness.violation.is_some(), "{liveness:#?}");
}

/// S1 is not vacuous: deferring an ambiguous failure leaves the placement waiting although
/// its effect may already have happened, which `EffectsFollowOutcome` rejects at once.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_retrying_an_ambiguous_failure_duplicates_the_effect() {
    assert_mutation_violates(
        Churn {
            ambiguities: 1,
            ..QUIET
        },
        Mutation::RetryAmbiguous,
        LawName::EffectsFollowOutcome,
    );
}

/// S2 is not vacuous: waking without the trigger retries a stale hop.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_waking_without_the_trigger_retries_a_stale_hop() {
    assert_mutation_violates(
        Churn {
            disconnects: 1,
            ..QUIET
        },
        Mutation::WakeUntriggered,
        LawName::FreshHop,
    );
}
