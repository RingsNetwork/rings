//! Stage 6 of the DHT model checks: Chord topology safety through
//! disconnect/rejoin races across connection generations, and conditional
//! convergence once churn stops (issue #772).
//!
//! The scoped stages in `tests::default::test_dht_stateright` check the
//! topology protocol and the finger scheduler; `pending::test_lifecycle_model`
//! checks one peer's lifecycle phases. This stage checks their *composition*:
//! a peer disconnects, is removed, rejoins under a newer local generation, and
//! work bound to the retired generation arrives afterwards. It lives here, not
//! beside the other stages, because its carrier holds the production
//! `ConnectionLifecycleRegistry`, which is visible only inside
//! `swarm::transport`.
//!
//! # Specification (TLA+ style)
//!
//! ```text
//! CONSTANTS  Ring (evenly spaced identities), K (successor capacity),
//!            W (finger slots), Budget (departures, rejoins, cuts, loss,
//!            duplication, refusal)
//!
//! VARIABLES  nodes    : Ring ⇀ [ topology  : TopologyState            (production)
//!                              , lifecycles: ConnectionLifecycleRegistry (production)
//!                              , callbacks : SUBSET Callback ]
//!            links    : SUBSET Link          \* physical connections, one generation per end
//!            network  : SUBSET Envelope      \* a set: any order, any delay
//!            remaining: Budget
//!            stale    : history variable of the retired-generation law
//!
//! Init  ≜ every peer up ∧ full mesh of admitted links ∧ Converged
//!
//! Next  ≜ Env ∨ Protocol
//! Env   ≜ Depart(p) ∨ Rejoin(p) ∨ Cut(l) ∨ Lose(e) ∨ Duplicate(e)   \* each spends Budget
//! Protocol ≜ Dial(p, q)          \* isolated peer reserves a generation and offers
//!          ∨ Deliver(offer)      \* answer ⇒ link ∧ ChannelOpened at both ends; refuse ⇒ Closed
//!          ∨ Deliver(frame)      \* gated on the receiver's admitted generation
//!          ∨ Observe(p, cb(g))   \* ChannelOpened | SendTerminal | RetireUnavailable | Closed
//!          ∨ Stabilize(p)        \* BeginStabilize ⇒ query ⇒ report ⇒ claim, offers, Stabilize ⇒ notify
//!
//! Owns(n, g) ≜ lifecycles[n] records generation g for g.peer (any phase)
//!
//! Safety (□):
//!   RetiredGenerationsAreInert      ≜ an event bound to g with ¬Owns(n, g) leaves
//!                                      (topology[n], lifecycles[n]) unchanged
//!   TopologiesAreWellFormed         ≜ ∀n. SuccessorsWellFormed(n, K) ∧ PredecessorWellFormed(n)
//!                                      ∧ FingersWellFormed(n)
//!   RoutingAdvancesClockwise        ≜ ∀n, id ∈ Ring. RoutesClockwise(n, id)
//!   TopologyReferencesOnlyAdmitted  ≜ ∀n, p. Referenced(n, p) ⇒ Active(n, p)
//!
//! Liveness:
//!   Fairness ≜ ∀a ∈ Protocol. WF(a)        \* no Env step in the suffix
//!   ∀r reachable. RetainsSuccessorPaths(r) ⇒ (□[Protocol] ∧ Fairness ⇒ ◇□Converged)
//!   Converged ≜ ∀n up. ChordFixpoint(topology[n], up peers, K)
//! ```
//!
//! # Fidelity
//!
//! - Every topology change is `topology::step`; every lifecycle change is a
//!   method of the production registry; the candidate budget of a report is
//!   the production `StabilizationConnectionPlan`; report confirmation is
//!   `TopoInfo::confirmed_by`. The carrier stores those production values, so
//!   there is no snapshot or shadow state machine to keep equal to them.
//! - What the model owns is the composition `SwarmTransport` performs under
//!   its lifecycle boundary (`node`), the physical world (`overlay`), and the
//!   search (`search`).
//!   Each composed step names the production path it interprets. The one
//!   place it hands `step` a differently shaped argument than production does
//!   (replacement candidates) is proved observationally equal below.
//! - The model does not distinguish wasm from native peers: a rejoin is a
//!   lifecycle transition of an ordinary peer. The search itself is plain
//!   single-threaded Rust, so the browser test job runs the same exhaustive
//!   checks against the wasm build of the production transitions; only the
//!   shell conformance test needs the native `SwarmTransport` harness.
//!
//! # Scope limits
//!
//! - No claim under endless churn, permanent partition, or Byzantine peers.
//! - Remote finger ranges, the finger scheduler, and the per-candidate
//!   revalidation of a connection plan are Stage 5 (`finger_retry_model`);
//!   identities are evenly spaced so every modeled finger's fixpoint is the
//!   successor head, the range stabilization itself proves.
//! - Handshake expiry, connection-capacity eviction, successor-list sync,
//!   connect lookups, storage repair, and bootstrap redial pacing (#763) are
//!   not modeled. Signaling is reliable; loss and duplication apply to frames.
//! - At most one stabilization round is in flight overlay-wide, a report is
//!   claimed and committed in one step, and transport readiness is identified
//!   with the registry's sendable projection.
//!
//! # Bounds
//!
//! Every search is exhaustive: there is no depth cut-off, and it ends when
//! the budgets are spent and the protocol closure is complete. `W = 3` finger
//! slots throughout. One breadth-first exploration per configuration decides
//! safety, coverage, and liveness; each test asserts its exact state count,
//! so a carrier that grows fails deterministically instead of slowing CI
//! silently. Depth is the longest breadth-first level.
//!
//! | configuration | peers | K | budget                                  | states    | depth | checked            |
//! |---------------|-------|---|-----------------------------------------|-----------|-------|--------------------|
//! | departure     | 3     | 1 | 1 departure                             | 5 834     | 28    | premise necessity  |
//! | truncation    | 4     | 2 | 1 cut                                   | 233 488   | 36    | all laws           |
//! | restart       | 3     | 1 | 1 departure, 1 rejoin                   | 552 309   | 47    | all laws           |
//! | lossy flap    | 3     | 1 | 1 cut, 1 loss, 1 duplication, 1 refusal | 1 057 016 | 42    | all laws, release  |
//!
//! The premise of the liveness claim is necessary, not decorative. When a
//! peer's only successor goes down and its close callback wins the race with
//! the unavailable-peer sweep, the removal preserves an empty successor list
//! (`SuccessorRemoval::Preserve`) although other admitted connections remain,
//! and nothing in the stabilization protocol refills it; that peer has no
//! successor path and `test_without_the_successor_path_premise_a_suffix_starves`
//! exhibits the starved suffix. Re-joining such a peer is the subject of #775.
//!
//! CI wall-clock limit: 10 minutes for this module in every test job.
//! Measured on an Apple M-series laptop, one search per test, tests in
//! parallel: native `--release` 52 s for the module (restart 13 s and
//! 450 MB peak, truncation 8 s and 245 MB, lossy flap 30 s and 625 MB);
//! native dev profile 139 s (lossy flap skipped, restart 135 s of it);
//! headless Chrome (`wasm32`, release, tests
//! run serially) 37 s for the module.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
mod conformance;
mod laws;
mod node;
mod overlay;
mod search;

use node::Callback;
use overlay::Budgets;
use overlay::Overlay;
use overlay::OverlayAction;
use overlay::OverlayState;
use overlay::ShellMutation;
use search::check;
use search::Verdict;

use crate::dht::topology::step;
use crate::dht::topology::successors;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::Did;

/// `|G|` of the `restart` configuration.
const RESTART_STATES: usize = 552_309;
/// `|G|` of the `truncation` configuration.
const TRUNCATION_STATES: usize = 233_488;
/// `|G|` of the `lossy flap` configuration.
const LOSSY_FLAP_STATES: usize = 1_057_016;
/// `|G|` of the `departure` configuration.
const DEPARTURE_STATES: usize = 5_834;

/// Finger slots of every modeled table.
const FINGER_SLOTS: usize = 3;

/// No adversarial budget: the base every configuration extends.
const QUIET: Budgets = Budgets {
    departures: 0,
    rejoins: 0,
    cuts: 0,
    loss: 0,
    duplication: 0,
    refusal: 0,
};

/// Three peers, `K = 1`: one peer goes down for good. The smallest
/// configuration in which a survivor can lose its only successor path.
fn departure(mutation: ShellMutation) -> Overlay {
    let budgets = Budgets {
        departures: 1,
        ..QUIET
    };
    Overlay::new(Did::from(0u32), 3, 1, FINGER_SLOTS, budgets, mutation)
}

/// Three peers, `K = 1`: one peer goes down and starts again, so the same
/// identity is re-admitted under a newer generation while the events of its
/// retired generation are still undelivered.
fn restart(mutation: ShellMutation) -> Overlay {
    let budgets = Budgets {
        departures: 1,
        rejoins: 1,
        ..QUIET
    };
    Overlay::new(Did::from(0u32), 3, 1, FINGER_SLOTS, budgets, mutation)
}

/// Four peers, `K = 2`: every peer has three eligible successors, one more
/// than capacity, so admission truncates and head replacement chooses.
fn truncation(mutation: ShellMutation) -> Overlay {
    let budgets = Budgets { cuts: 1, ..QUIET };
    Overlay::new(Did::from(0u32), 4, 2, FINGER_SLOTS, budgets, mutation)
}

/// Four peers, `K = 2`: one peer goes down and one link dies, so a head is
/// replaced while a ring member holds no admitted generation. Used only with
/// a mutated shell, whose counterexample is shallow.
fn departure_then_flap(mutation: ShellMutation) -> Overlay {
    let budgets = Budgets {
        departures: 1,
        cuts: 1,
        ..QUIET
    };
    Overlay::new(Did::from(0u32), 4, 2, FINGER_SLOTS, budgets, mutation)
}

/// Three peers, `K = 1`: one link flaps while the network may lose one frame,
/// duplicate one, and deliver one offer to a peer that refuses it.
fn lossy_flap(mutation: ShellMutation) -> Overlay {
    let budgets = Budgets {
        cuts: 1,
        loss: 1,
        duplication: 1,
        refusal: 1,
        ..QUIET
    };
    Overlay::new(Did::from(0u32), 3, 1, FINGER_SLOTS, budgets, mutation)
}

/// Decide every law and the conditional-liveness claim over the complete
/// reachable graph of `overlay`, whose size must be exactly `states`, and
/// require that neither claim was vacuous.
fn assert_laws_hold_exhaustively(name: &str, overlay: &Overlay, states: usize) {
    let verdict = check(overlay, laws::retains_successor_paths);
    println!(
        "{name}: {} states, depth {}, {} stable, {} premise states ({} not yet stable)",
        verdict.states,
        verdict.max_depth,
        verdict.stable_states,
        verdict.premise_states,
        verdict.unstable_premise_states,
    );
    assert!(verdict.safety.is_none(), "{name}: {:#?}", verdict.safety);
    assert!(
        verdict.uncovered.is_empty(),
        "{name}: {:?}",
        verdict.uncovered
    );
    assert_eq!(verdict.states, states, "{name}: stale bounds table");
    assert!(
        verdict.unstable_premise_states > 0,
        "{name}: vacuous premise"
    );
    assert!(verdict.stable_states > 0, "{name}: unreachable target");
    assert!(
        verdict.liveness.is_none(),
        "{name}: {:#?}",
        verdict.liveness
    );
}

/// Apply `action`, requiring that the model enables it: a scripted trace is a
/// behaviour of the model, not a sequence of forced writes.
fn enabled_step(overlay: &Overlay, state: &OverlayState, action: OverlayAction) -> OverlayState {
    assert!(
        overlay.actions(state).contains(&action),
        "not enabled: {action:?}"
    );
    overlay
        .next_state(state, action.clone())
        .unwrap_or_else(|| panic!("no state change: {action:?}"))
}

/// Deterministic replay: fold a recorded trace from `Init`.
fn replay(overlay: &Overlay, trace: &[OverlayAction]) -> OverlayState {
    trace
        .iter()
        .cloned()
        .fold(overlay.converged_mesh(), |state, action| {
            enabled_step(overlay, &state, action)
        })
}

/// The minimal counterexample to the law `name` under a mutated shell.
fn minimal_counterexample(overlay: &Overlay, name: &str) -> Vec<OverlayAction> {
    let Verdict { safety, .. } = check(overlay, laws::retains_successor_paths);
    let violation = safety.unwrap_or_else(|| panic!("the mutated shell must violate: {name}"));
    assert_eq!(violation.law, name);
    violation.trace
}

/// Law: `Init` is the Chord fixpoint of the whole ring, satisfies the
/// liveness premise, and has nothing in flight.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_initial_mesh_is_the_converged_fixpoint() {
    for overlay in [
        restart(ShellMutation::Faithful),
        truncation(ShellMutation::Faithful),
    ] {
        let init = overlay.converged_mesh();
        assert!(laws::is_converged(&overlay, &init));
        assert!(laws::retains_successor_paths(&init));
        assert!(init.network.is_empty());
        assert!(init.nodes.values().all(|node| node.callbacks.is_empty()));
    }
}

/// Every law and conditional liveness over every behaviour in which a peer
/// restarts: the retired generation is inert, topologies stay well formed,
/// routing advances, topology evidence is backed by admitted generations,
/// and every fair churn-free suffix that retains successor paths reaches,
/// and stays in, the Chord fixpoint of the live set.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_laws_hold_when_a_peer_restarts_under_a_newer_generation() {
    assert_laws_hold_exhaustively("restart", &restart(ShellMutation::Faithful), RESTART_STATES);
}

/// Every law and conditional liveness with more eligible successors than
/// capacity.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_laws_hold_when_successors_exceed_capacity() {
    assert_laws_hold_exhaustively(
        "truncation",
        &truncation(ShellMutation::Faithful),
        TRUNCATION_STATES,
    );
}

/// Every law and conditional liveness with frame loss, duplication,
/// reordering, and a refused offer: a lost query or report is superseded by
/// the next round.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
#[cfg_attr(
    debug_assertions,
    ignore = "a million-state graph: decided by the release test jobs"
)]
fn test_laws_hold_under_loss_duplication_and_refusal() {
    assert_laws_hold_exhaustively(
        "lossy flap",
        &lossy_flap(ShellMutation::Faithful),
        LOSSY_FLAP_STATES,
    );
}

/// The acceptance trace, step by step: disconnect, removal with successor
/// replacement, restart, re-admission under a newer generation, and then the
/// retired generation's close, which must change nothing.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_retired_close_after_rejoin_changes_neither_lifecycle_nor_topology() {
    let overlay = restart(ShellMutation::Faithful);
    let [observer, departed, _] = <[Did; 3]>::try_from(overlay.ring().to_vec()).unwrap();
    let init = overlay.converged_mesh();
    let retired = init.nodes[&observer]
        .lifecycles
        .active_attempt(departed)
        .unwrap();
    let observe = |callback| OverlayAction::Observe {
        node: observer,
        callback,
    };

    let down = enabled_step(&overlay, &init, OverlayAction::Depart(departed));
    let terminal = enabled_step(&overlay, &down, observe(Callback::SendTerminal(retired)));
    let removed = enabled_step(
        &overlay,
        &terminal,
        observe(Callback::RetireUnavailable(retired)),
    );
    let after_removal = &removed.nodes[&observer];
    assert!(!after_removal.owns(retired));
    assert!(!after_removal.topology.references(departed));
    assert!(after_removal
        .topology
        .successors_are_well_formed(overlay.successor_capacity()));

    let up = enabled_step(&overlay, &removed, OverlayAction::Rejoin(departed));
    let dialed = enabled_step(&overlay, &up, OverlayAction::Dial {
        from: departed,
        to: observer,
    });
    let offer = dialed.network.first().cloned().unwrap();
    let answered = enabled_step(&overlay, &dialed, OverlayAction::Deliver(offer));
    let newer = answered.nodes[&observer]
        .lifecycles
        .unadmitted_attempt(departed)
        .unwrap();
    assert!(newer.generation > retired.generation);
    let admitted = enabled_step(&overlay, &answered, observe(Callback::ChannelOpened(newer)));
    assert!(admitted.nodes[&observer].topology.references(departed));

    let stale = enabled_step(&overlay, &admitted, observe(Callback::Closed(retired)));
    assert_eq!(
        stale.nodes[&observer].protected(),
        admitted.nodes[&observer].protected(),
        "the retired generation's close must be inert"
    );
    assert_eq!(stale.stale_effect, None);
}

/// Non-vacuity of `RetiredGenerationsAreInert`: with the exact-generation
/// guard removed, the search finds a minimal trace in which a retired
/// generation's callback revokes the newer one, and the trace replays.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_removing_the_generation_guard_yields_a_replayable_counterexample() {
    let overlay = restart(ShellMutation::CallbackIgnoresGeneration);
    let trace = minimal_counterexample(&overlay, laws::RETIRED_GENERATIONS_ARE_INERT);
    println!("generation guard removed: {trace:#?}");
    let witness = replay(&overlay, &trace).stale_effect.unwrap();
    assert!(
        matches!(
            trace.last(),
            Some(OverlayAction::Observe { node, callback })
                if *node == witness.node && callback.attempt() == witness.retired
        ),
        "the last step delivers the retired generation's callback"
    );
    assert_eq!(
        replay(&restart(ShellMutation::Faithful), &trace).stale_effect,
        None,
        "the same trace is inert under the production guard"
    );
}

/// Non-vacuity of `TopologyReferencesOnlyAdmitted`: with head replacement
/// drawing on peers that hold no admitted generation, the search finds a
/// minimal trace that installs unvalidated successor evidence.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_removing_the_replacement_invariant_yields_a_replayable_counterexample() {
    let overlay = departure_then_flap(ShellMutation::ReplacementIgnoresLifecycle);
    let trace = minimal_counterexample(&overlay, laws::TOPOLOGY_REFERENCES_ONLY_ADMITTED);
    println!("replacement invariant removed: {trace:#?}");
    let violated = replay(&overlay, &trace);
    assert!(violated.nodes.values().any(|node| {
        node.topology
            .referenced_peers()
            .into_iter()
            .any(|peer| node.lifecycles.active_attempt(peer).is_none())
    }));
}

/// Law: `Remove{ReplaceWith(X)} = Remove{ReplaceWith(Successors(X, n, K))}`.
///
/// Production normalizes the replacement candidates before the transition
/// and the transition normalizes them again; the model passes them
/// unnormalized. Both agree on every candidate subset and every removed peer,
/// so the model's argument shape is unobservable.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_replacement_normalization_is_absorbed_by_the_production_remove() {
    let overlay = truncation(ShellMutation::Faithful);
    let capacity = overlay.successor_capacity();
    let (local, peers) = overlay.ring().split_first().unwrap();
    for removed in peers.iter().copied() {
        for mask in 0u32..(1 << peers.len()) {
            let candidates = peers
                .iter()
                .enumerate()
                .filter(|(bit, peer)| mask & (1 << bit) != 0 && **peer != removed)
                .map(|(_, peer)| *peer)
                .collect::<Vec<_>>();
            let state = TopologyState::new(*local, vec![removed], None, vec![None; FINGER_SLOTS]);
            let remove = |replacements| TopologyEvent::Remove {
                peer: removed,
                successor: SuccessorRemoval::ReplaceWith(replacements),
            };
            assert_eq!(
                step(&state, remove(candidates.clone()), capacity),
                step(
                    &state,
                    remove(successors(&candidates, *local, capacity)),
                    capacity
                ),
            );
        }
    }
}

/// Non-vacuity of the liveness analysis and necessity of its premise: with
/// the premise dropped, a peer whose only successor closed is left without a
/// successor path, and the analysis reports the starved suffix, which replays.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_without_the_successor_path_premise_a_suffix_starves() {
    let overlay = departure(ShellMutation::Faithful);
    let verdict = check(&overlay, |_| true);
    assert_eq!(verdict.states, DEPARTURE_STATES, "stale bounds table");
    let violation = verdict.liveness.unwrap();
    println!("premise dropped: {violation:#?}");
    assert!(violation
        .quiescent_suffix
        .iter()
        .all(|action| !action.is_environmental()));
    let trace = [violation.churn_prefix, violation.quiescent_suffix].concat();
    let starved = replay(&overlay, &trace);
    assert!(!laws::is_converged(&overlay, &starved));
    assert!(!laws::retains_successor_paths(&starved));
}
