//! Exhaustive model check of the rerouting automaton (#859), on the search shared with the
//! Chord rejoin model (`test_model_check`, #772).
//!
//! # Specification (TLA+ style)
//!
//! ```text
//! CONSTANTS  B = REROUTING_BUDGET, Churn (reservations, glare, withdrawals, deaths,
//!            disconnects, reroutes, congestions, ambiguities)
//!
//! VARIABLES  registry   : ConnectionLifecycleRegistry of the target hop   (production)
//!            ready      : BOOLEAN                \* the admitted generation can make progress
//!            preference : {Target, Alternate, Local}
//!            congested  : BOOLEAN ; capacity : ℕ  \* local capacity and its release epoch
//!            phase      : Compute(d) | InFlight(d, hop, g, stamp)
//!                       | Waiting(d, hop, stamp, cause) | Done(outcome)
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
//! Liveness:    Fairness ≜ WF(Admit) ∧ WF(Recover) ∧ WF(Close) ∧ WF(Release)
//!                         ∧ WF(Send) ∧ WF(Accept) ∧ WF(Refuse(r)) ∧ WF(Wake)
//!              ∀r. Premise(r) ⇒ (□[Protocol] ∧ Fairness ⇒ ◇ Done(Accepted))      (L1)
//!              Premise(r) ≜ phase ∉ Done ∧ deferrals ≤ B − QUIESCENT_DEFERRALS
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
//! reroutes `t`, congestions `c`, ambiguities `a`.
//!
//! | configuration       | r g w d x t c a | states | depth | premise (unsettled) |
//! |---------------------|-----------------|--------|-------|---------------------|
//! | replacement         | 2 0 0 2 1 0 0 1 | 1642   | 21    | 1048 (1048)         |
//! | glare               | 2 1 0 1 0 0 1 0 | 957    | 18    | 765 (765)           |
//! | retire before ready | 2 0 1 2 0 0 0 0 | 604    | 16    | 479 (479)           |
//! | topology mid-wait   | 1 0 0 1 1 2 1 0 | 21417  | 27    | 13977 (13977)       |
//! | exhaustion          | 2 0 0 2 2 0 2 0 | 49702  | 33    | 26675 (26675)       |

mod carrier;
mod laws;
mod next;

use carrier::Churn;
use carrier::Model;
use carrier::Mutation;
use laws::has_liveness_budget;
use laws::LawName;

use super::QUIESCENT_DEFERRALS;
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
    ambiguities: 0,
};

/// The topology-mid-wait churn: the route moves while a placement waits.
const TOPOLOGY_MID_WAIT: Churn = Churn {
    reservations: 1,
    deaths: 1,
    reroutes: 2,
    congestions: 1,
    disconnects: 1,
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
            states: 1642,
            depth: 21,
            premise_states: 1048,
            unstable_premise_states: 1048,
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
            states: 957,
            depth: 18,
            premise_states: 765,
            unstable_premise_states: 765,
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
            states: 604,
            depth: 16,
            premise_states: 479,
            unstable_premise_states: 479,
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
            states: 21417,
            depth: 27,
            premise_states: 13977,
            unstable_premise_states: 13977,
        },
        &[
            LawName::WaitEndsByRouteChange,
            LawName::WaitEndsByCapacityRelease,
        ],
    );
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
            ..QUIET
        },
        Bounds {
            states: 49702,
            depth: 33,
            premise_states: 26675,
            unstable_premise_states: 26675,
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

/// S1 is not vacuous: deferring an ambiguous failure leaves an effect behind a pending retry
/// (`EffectsFollowOutcome`, the earliest witness) that a second acceptance then duplicates.
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
