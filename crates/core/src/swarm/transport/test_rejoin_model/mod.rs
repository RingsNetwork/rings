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
//!                              , events    : SUBSET LifecycleEvent
//!                              , unreplaced_head : history variable ]
//!            links    : SUBSET Link          \* physical connections, one generation per end
//!            network  : SUBSET Envelope      \* a set: any order, any delay
//!            remaining: Budget
//!            stale    : history variable of the retired-generation law
//!
//! Init  ≜ every peer up ∧ (full mesh of admitted links ∧ Converged
//!                          ∨ chain of admitted links, ring[i] dialed ring[i-1])
//!
//! Next  ≜ Env ∨ Protocol
//! Env   ≜ Depart(p) ∨ Rejoin(p) ∨ Cut(l) ∨ Lose(e) ∨ Duplicate(e)   \* each spends Budget
//! Protocol ≜ Dial(p, q)          \* isolated peer reserves a generation and offers
//!          ∨ Deliver(offer)      \* void ⇒ dropped; answer ⇒ link ∧ ChannelOpened at both ends;
//!                                \* refuse ⇒ Closed, spending Budget.refusal
//!          ∨ Deliver(message)    \* gated on the receiver's admitted generation
//!          ∨ Observe(p, ev(g))   \* ChannelOpened | SendTerminal | RetireUnavailable | Closed
//!          ∨ Stabilize(p)        \* BeginStabilize ⇒ query ⇒ report ⇒ claim, offers, Stabilize ⇒ notify
//!
//! Owns(n, g) ≜ lifecycles[n] records generation g for g.peer (any phase)
//!
//! Safety (□):
//!   RetiredGenerationsAreInert      ≜ an event bound to g with ¬Owns(n, g) leaves
//!                                      (topology[n], lifecycles[n]) unchanged
//!   UnavailableHeadsAreReplaced     ≜ retiring the head as unavailable leaves
//!                                      succ[n] = Successors(Sendable(n) ∖ {head}, n, K)
//!   TopologyReferencesOnlyAdmitted  ≜ ∀n, p. Referenced(n, p) ⇒ Active(n, p)
//!   TopologiesAreWellFormed         ≜ ∀n. SuccessorsWellFormed(n, K) ∧ PredecessorWellFormed(n)
//!                                      ∧ FingersWellFormed(n)
//!   StorageRepairFollowsVacatedSlots ≜ a retirement requests the storage repair round
//!                                      ⟺ Slots(topology'[n]) ≠ Slots(topology[n])
//!
//! Liveness:
//!   Fairness ≜ ∀a ∈ {Dial(p,q), Deliver(e), Observe(p,ev)}. WF(a)   \* per instance
//!            ∧ ∀p. SF(Stabilize(p))                                  \* no Env step in the suffix
//!   ∀r reachable. RetainsLiveHeads(r) ⇒ (□[Protocol] ∧ Fairness ⇒ ◇□Converged)
//!   Converged ≜ ∀n up. ChordFixpoint(topology[n], up peers, K)
//! ```
//!
//! Fairness is per action instance: every buffered frame, every queued
//! lifecycle event, and every isolated peer's dial is eventually processed,
//! which is what production's queues provide. `Stabilize` is strongly fair
//! because the search allows one round in flight overlay-wide: the
//! production timer is continuously enabled, and weak fairness of a
//! continuously enabled action is strong fairness of the same action under
//! a bound that sometimes disables it (`search`).
//!
//! # Fidelity
//!
//! - Every topology change is `topology::step`; every lifecycle change is a
//!   method of the production registry; the candidate budget of a report is
//!   the production `ConnectionPlan`; report confirmation is
//!   `TopoInfo::confirmed_by`; the removal flavours are the production
//!   `DhtPeerRemoval`. The carrier stores those production values, so there
//!   is no snapshot or shadow state machine to keep equal to them.
//! - What the model owns is the composition `SwarmTransport` performs under
//!   its lifecycle boundary (`node`), the physical world (`overlay`), and the
//!   search (`search`). Each composed step names the production path it
//!   interprets. The one place it hands `step` a differently shaped argument
//!   than production does (replacement candidates) is proved observationally
//!   equal below, and the acceptance trace is replayed against a real
//!   `SwarmTransport` (`conformance`), including the unavailable-head
//!   replacement under the dummy transport.
//! - The model does not distinguish wasm from native peers: a rejoin is a
//!   lifecycle transition of an ordinary peer. The search itself is plain
//!   single-threaded Rust, so the browser test job runs the same exhaustive
//!   checks against the wasm build of the production transitions; only the
//!   shell conformance tests need the native `SwarmTransport` harness.
//!
//! # Scope limits
//!
//! - No claim under endless churn, permanent partition, or Byzantine peers.
//! - Remote finger ranges, the finger scheduler, the per-candidate
//!   revalidation of a connection plan, and stale stabilization tokens across
//!   rounds are Stage 5 (`finger_retry_model`); identities are evenly spaced
//!   so every modeled finger's fixpoint is the successor head, the range
//!   stabilization itself proves.
//! - Not modeled: connection-capacity eviction, successor-list sync, connect
//!   lookups, the storage repair round itself (its *request* is the
//!   `StorageRepair` effect of a retirement, production's `TopologyRemoval`
//!   of the removed state), and
//!   bootstrap redial pacing and seed selection (#763): an isolated peer
//!   dials any live peer. Handshake expiry appears only as the close of a
//!   refused offer's generation. Signaling is reliable; loss and duplication
//!   apply to protocol messages, and a message on a link that dies is lost
//!   with it.
//! - At most one stabilization round is in flight overlay-wide; a report is
//!   claimed and committed in one step; transport readiness is identified
//!   with the registry's sendable projection, so a peer may be confirmed or
//!   admitted on a link that is already dead and retired on the queued
//!   close.
//!
//! # Bounds
//!
//! Every search is exhaustive: there is no depth cut-off, and it ends when
//! the budgets are spent and the protocol closure is complete. `W = 3` finger
//! slots throughout the searches (the conformance replays use the production
//! sizes). One breadth-first exploration per configuration decides safety,
//! coverage, and liveness; each test asserts its exact state count, depth,
//! and premise counts, so a carrier that grows fails deterministically
//! instead of slowing CI silently, and the search aborts above
//! `EXPLORATION_BOUND` (two million states).
//!
//! | configuration | peers | K | budget                                  | states    | depth | premise states (unsettled) | checked            |
//! |---------------|-------|---|-----------------------------------------|-----------|-------|----------------------------|--------------------|
//! | departure     | 3     | 1 | 1 departure                             | 4 862     | 21    | —                          | premise necessity  |
//! | chain         | 4     | 2 | none (chain bootstrap)                  | 509 815   | 60    | 509 815 (495 191)          | see below          |
//! | replacement   | 4     | 1 | 1 cut                                   | 109 840   | 35    | 91 312 (25 472)            | all laws           |
//! | truncation    | 4     | 2 | 1 cut                                   | 237 328   | 36    | 219 664 (174 048)          | all laws           |
//! | restart       | 3     | 1 | 1 departure, 1 rejoin                   | 542 589   | 47    | 464 848 (408 404)          | all laws           |
//! | lossy flap    | 3     | 1 | 1 cut, 1 loss, 1 duplication, 1 refusal | 1 034 264 | 42    | 890 762 (653 760)          | all laws, release  |
//!
//! "Premise states" counts the reachable states from which the liveness
//! claim is made; "unsettled" those of them outside `Stable`, where the
//! claim has work to do. `replacement` is the configuration in which a head
//! replacement has more sendable candidates than capacity; `truncation`
//! truncates at admission and fills exactly at replacement. `chain` starts
//! from a chain of links instead of the converged mesh, so the missing
//! successors are discovered through reports and the connection plans they
//! drive, then installed by admission; it checks `TopologyReferencesOnlyAdmitted`,
//! well-formedness, and liveness, while its retirement laws and churn
//! witnesses are vacuous by construction (no generation is ever retired) and
//! the witnesses are asserted uncovered. It does not distinguish the #786
//! fix from the defect: a confirmed report entry that ranks within capacity
//! is already admitted, so #786 is pinned by its unit law alone.
//!
//! The premise of the liveness claim is necessary, not decorative. When a
//! peer's only successor goes down and its close event wins the race with
//! the unavailable-peer sweep, the removal preserves an empty successor list
//! (`SuccessorRemoval::Preserve`) although other admitted connections remain,
//! and nothing in the stabilization protocol refills it; that peer has no
//! live head and `test_without_the_live_head_premise_a_suffix_starves`
//! exhibits the starved suffix. Re-joining such a peer is the subject of #775.
//!
//! Wall clock: no CI job bounds a single test of this module; the binding
//! limits are the job timeouts (45 minutes) and, in the search itself,
//! `EXPLORATION_BOUND`. The browser runner's `WASM_BINDGEN_TEST_TIMEOUT`
//! (120 s) applies to asynchronous tests; these searches are synchronous.
//! Measured on an Apple M-series laptop with the tests in parallel: native
//! dev profile, the whole core suite with lossy flap skipped, 249 s (this
//! module's searches are its longest tests; with lossy flap included the
//! module alone is about 7 minutes); headless Chrome (`wasm32`, release,
//! tests run serially, lossy flap included) 54 s for the module's sixteen
//! tests. Native release timings, measured on an earlier revision with the
//! same configurations up to a few percent of states: restart 13 s and
//! 450 MB peak, truncation 8 s and 245 MB, lossy flap 30 s and 625 MB.
//! Browser peak memory was not measured.

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
mod conformance;
mod laws;
mod node;
mod overlay;
mod search;

use std::num::NonZeroU32;

use laws::LawName;
use node::LifecycleEvent;
use node::MisdirectedRepair;
use node::UnreplacedHead;
use overlay::Bootstrap;
use overlay::Budget;
use overlay::Overlay;
use overlay::OverlayAction;
use overlay::OverlayState;
use overlay::ShellMutation;
use search::check;
use search::LivenessAnalysis;
use search::SearchReport;

use crate::dht::topology::step;
use crate::dht::topology::successors;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::Did;
use crate::swarm::transport::pending::PendingConnectionAttempt;

/// The documented size of one configuration's reachable graph, asserted
/// exactly so the bounds table cannot drift.
struct Bounds {
    /// `|G|`.
    states: usize,
    /// Longest breadth-first level.
    depth: usize,
    /// States satisfying the liveness premise.
    premise_states: usize,
    /// Premise states outside `Stable`.
    unstable_premise_states: usize,
}

/// Bounds of the `restart` configuration.
const RESTART_BOUNDS: Bounds = Bounds {
    states: 542_589,
    depth: 47,
    premise_states: 464_848,
    unstable_premise_states: 408_404,
};
/// Bounds of the `truncation` configuration.
const TRUNCATION_BOUNDS: Bounds = Bounds {
    states: 237_328,
    depth: 36,
    premise_states: 219_664,
    unstable_premise_states: 174_048,
};
/// Bounds of the `lossy flap` configuration.
const LOSSY_FLAP_BOUNDS: Bounds = Bounds {
    states: 1_034_264,
    depth: 42,
    premise_states: 890_762,
    unstable_premise_states: 653_760,
};
/// Bounds of the `replacement` configuration.
const REPLACEMENT_BOUNDS: Bounds = Bounds {
    states: 109_840,
    depth: 35,
    premise_states: 91_312,
    unstable_premise_states: 25_472,
};
/// Bounds of the `chain` configuration.
const CHAIN_BOUNDS: Bounds = Bounds {
    states: 509_815,
    depth: 60,
    premise_states: 509_815,
    unstable_premise_states: 495_191,
};
/// `(|G|, depth)` of the `departure` configuration.
const DEPARTURE_BOUNDS: (usize, usize) = (4_862, 21);

/// Finger slots of every modeled table.
const FINGER_SLOTS: usize = 3;

/// No adversarial budget: the base every configuration extends.
const QUIET: Budget = Budget {
    departures: 0,
    rejoins: 0,
    cuts: 0,
    loss: 0,
    duplication: 0,
    refusal: 0,
};

/// A ring at the origin with `peers` identities, successor capacity `k`,
/// and the modeled finger width.
fn configuration(
    peers: u32,
    k: usize,
    budget: Budget,
    bootstrap: Bootstrap,
    mutation: ShellMutation,
) -> Overlay {
    let peers = NonZeroU32::new(peers).unwrap_or(NonZeroU32::MIN);
    Overlay::new(
        Did::from(0u32),
        peers,
        k,
        FINGER_SLOTS,
        budget,
        bootstrap,
        mutation,
    )
}

/// Four peers, `K = 2`, bootstrapped as a chain with no churn: the peers
/// missing from every successor list are discovered only through reports
/// and the connection plans they drive.
fn chain(mutation: ShellMutation) -> Overlay {
    configuration(4, 2, QUIET, Bootstrap::Chain, mutation)
}

/// Three peers, `K = 1`: one peer goes down for good. The smallest
/// configuration in which a survivor can lose its only successor.
fn departure(mutation: ShellMutation) -> Overlay {
    let budget = Budget {
        departures: 1,
        ..QUIET
    };
    configuration(3, 1, budget, Bootstrap::ConvergedMesh, mutation)
}

/// Three peers, `K = 1`: one peer goes down and starts again, so the same
/// identity is re-admitted under a newer generation while the events of its
/// retired generation are still undelivered.
fn restart(mutation: ShellMutation) -> Overlay {
    let budget = Budget {
        departures: 1,
        rejoins: 1,
        ..QUIET
    };
    configuration(3, 1, budget, Bootstrap::ConvergedMesh, mutation)
}

/// Four peers, `K = 2`: every peer has three eligible successors, one more
/// than capacity, so admission truncates and head replacement chooses.
fn truncation(mutation: ShellMutation) -> Overlay {
    configuration(
        4,
        2,
        Budget { cuts: 1, ..QUIET },
        Bootstrap::ConvergedMesh,
        mutation,
    )
}

/// Four peers, `K = 1`: a cut head has two sendable candidates for one
/// slot, so head replacement truncates.
fn replacement(mutation: ShellMutation) -> Overlay {
    configuration(
        4,
        1,
        Budget { cuts: 1, ..QUIET },
        Bootstrap::ConvergedMesh,
        mutation,
    )
}

/// Three peers, `K = 1`: one link flaps while the network may lose one
/// message, duplicate one, and deliver one offer to a peer that refuses it.
fn lossy_flap(mutation: ShellMutation) -> Overlay {
    let budget = Budget {
        cuts: 1,
        loss: 1,
        duplication: 1,
        refusal: 1,
        ..QUIET
    };
    configuration(3, 1, budget, Bootstrap::ConvergedMesh, mutation)
}

/// The `Sometimes` laws a configuration cannot satisfy: those that witness
/// churn, in a configuration without any.
const CHURN_WITNESSES: [LawName; 2] = [
    LawName::RetiredEventAwaitsBesideNewerGeneration,
    LawName::HeadReplacementFillsCapacity,
];

/// Decide every law and the conditional-liveness claim over the complete
/// reachable graph of `overlay`, whose size, depth, and premise counts must
/// be exactly `bounds`, whose unsatisfied `Sometimes` laws must be exactly
/// `expected_uncovered`, and require that neither claim was vacuous.
fn assert_laws_hold_exhaustively(
    name: &str,
    overlay: &Overlay,
    bounds: Bounds,
    expected_uncovered: &[LawName],
) {
    let report = check(overlay, laws::retains_live_heads);
    let SearchReport::Safe {
        states,
        max_depth,
        uncovered,
        liveness,
    } = report
    else {
        panic!("{name}: {report:#?}");
    };
    let LivenessAnalysis {
        premise_states,
        unstable_premise_states,
        stable_states,
        violation,
    } = liveness;
    println!(
        "{name}: {states} states, depth {max_depth}, {stable_states} stable, \
         {premise_states} premise states ({unstable_premise_states} not yet stable)"
    );
    assert_eq!(uncovered, expected_uncovered, "{name}: coverage");
    assert_eq!(
        (states, max_depth, premise_states, unstable_premise_states),
        (
            bounds.states,
            bounds.depth,
            bounds.premise_states,
            bounds.unstable_premise_states
        ),
        "{name}: stale bounds table"
    );
    assert!(unstable_premise_states > 0, "{name}: vacuous premise");
    assert!(stable_states > 0, "{name}: unreachable target");
    assert!(violation.is_none(), "{name}: {violation:#?}");
}

/// The `Observe` action of `peer`, as a function of the event.
fn observed_at(peer: Did) -> impl Fn(LifecycleEvent) -> OverlayAction {
    move |event| OverlayAction::Observe { peer, event }
}

/// The model's rejoin of `departed` through `observer`: restart, bootstrap
/// dial, the offer's answer, and admission under the newer generation.
/// Returns the state after admission and the newer generation.
fn model_rejoin(
    overlay: &Overlay,
    state: &OverlayState,
    observer: Did,
    departed: Did,
) -> (OverlayState, PendingConnectionAttempt) {
    let up = enabled_step(overlay, state, OverlayAction::Rejoin(departed));
    let dialed = enabled_step(overlay, &up, OverlayAction::Dial {
        from: departed,
        to: observer,
    });
    let offer = dialed.network.first().cloned().unwrap();
    let answered = enabled_step(overlay, &dialed, OverlayAction::Deliver(offer));
    let newer = answered.nodes[&observer]
        .lifecycles
        .unadmitted_attempt(departed)
        .unwrap();
    let admitted = enabled_step(
        overlay,
        &answered,
        observed_at(observer)(LifecycleEvent::ChannelOpened(newer)),
    );
    (admitted, newer)
}

/// Apply `action`, requiring that the model enables it: a scripted trace is a
/// behaviour of the model, not a sequence of forced writes.
fn enabled_step(overlay: &Overlay, state: &OverlayState, action: OverlayAction) -> OverlayState {
    assert!(
        overlay.actions(state).contains(&action),
        "not enabled: {action:?}"
    );
    overlay
        .next_state(state, &action)
        .unwrap_or_else(|| panic!("no state change: {action:?}"))
}

/// Deterministic replay: fold a recorded trace from `Init`.
fn replay(overlay: &Overlay, trace: &[OverlayAction]) -> OverlayState {
    trace.iter().cloned().fold(overlay.init(), |state, action| {
        enabled_step(overlay, &state, action)
    })
}

/// The minimal counterexample to the law `name` under a mutated shell, which
/// must replay to a violation of that law and to none under the faithful
/// shell of the same configuration.
fn minimal_counterexample(
    mutated: &Overlay,
    faithful: &Overlay,
    name: LawName,
) -> Vec<OverlayAction> {
    let report = check(mutated, laws::retains_live_heads);
    let SearchReport::Unsafe { states, violation } = report else {
        panic!("the mutated shell must violate `{name}`: {report:#?}");
    };
    println!("`{name}` violated after {states} states: {violation:#?}");
    assert_eq!(violation.law, name, "{violation:#?}");
    let law = laws::LAWS
        .iter()
        .find(|law| law.name == name)
        .unwrap_or_else(|| panic!("unknown law {name}"));
    assert!(
        !(law.holds)(mutated, &replay(mutated, &violation.trace)),
        "the trace must replay to the violation: {violation:#?}"
    );
    assert!(
        (law.holds)(faithful, &replay(faithful, &violation.trace)),
        "the same trace must satisfy the law under the faithful shell: {violation:#?}"
    );
    violation.trace
}

/// Law: the mesh `Init` is the Chord fixpoint of the whole ring, and the
/// chain `Init` is not; both satisfy the liveness premise and have nothing
/// in flight.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_initial_states_satisfy_the_premise_and_only_the_mesh_is_converged() {
    for (overlay, converged) in [
        (restart(ShellMutation::Faithful), true),
        (truncation(ShellMutation::Faithful), true),
        (chain(ShellMutation::Faithful), false),
    ] {
        let init = overlay.init();
        assert_eq!(laws::is_converged(&overlay, &init), converged);
        assert!(laws::retains_live_heads(&init));
        assert!(init.network.is_empty());
        assert!(init.nodes.values().all(|node| node.events.is_empty()));
    }
}

/// Every law and conditional liveness over every behaviour in which a peer
/// restarts: the retired generation is inert, unavailable heads are
/// replaced, topology evidence is backed by admitted generations, topologies
/// stay well formed, and every fair churn-free suffix that retains live
/// heads reaches, and stays in, the Chord fixpoint of the live set.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_laws_hold_when_a_peer_restarts_under_a_newer_generation() {
    assert_laws_hold_exhaustively(
        "restart",
        &restart(ShellMutation::Faithful),
        RESTART_BOUNDS,
        &[],
    );
}

/// Every law and conditional liveness with more eligible successors than
/// capacity.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_laws_hold_when_successors_exceed_capacity() {
    assert_laws_hold_exhaustively(
        "truncation",
        &truncation(ShellMutation::Faithful),
        TRUNCATION_BOUNDS,
        &[],
    );
}

/// Every law and conditional liveness from a chain bootstrap: successors a
/// peer never dialed are discovered only through reports.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_laws_hold_from_a_chain_bootstrap() {
    assert_laws_hold_exhaustively(
        "chain",
        &chain(ShellMutation::Faithful),
        CHAIN_BOUNDS,
        &CHURN_WITNESSES,
    );
}

/// Every law and conditional liveness where head replacement truncates.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_laws_hold_when_head_replacement_truncates() {
    assert_laws_hold_exhaustively(
        "replacement",
        &replacement(ShellMutation::Faithful),
        REPLACEMENT_BOUNDS,
        &[],
    );
}

/// Every law and conditional liveness with message loss, duplication,
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
        LOSSY_FLAP_BOUNDS,
        &[],
    );
}

/// The acceptance trace, step by step: disconnect, removal with successor
/// replacement, restart, re-admission under a newer generation, and then the
/// retired generation's close, which must change nothing.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_retired_close_after_rejoin_changes_neither_lifecycle_nor_topology() {
    let overlay = restart(ShellMutation::Faithful);
    let [observer, departed, _] = <[Did; 3]>::try_from(overlay.ring()).unwrap();
    let init = overlay.init();
    let retired = init.nodes[&observer]
        .lifecycles
        .active_attempt(departed)
        .unwrap();
    let observe = observed_at(observer);

    let down = enabled_step(&overlay, &init, OverlayAction::Depart(departed));
    let terminal = enabled_step(
        &overlay,
        &down,
        observe(LifecycleEvent::SendTerminal(retired)),
    );
    let removed = enabled_step(
        &overlay,
        &terminal,
        observe(LifecycleEvent::RetireUnavailable(retired)),
    );
    let after_removal = &removed.nodes[&observer];
    assert!(!after_removal.owns(retired));
    assert!(!after_removal.topology.references(departed));
    assert!(after_removal
        .topology
        .is_well_formed(overlay.successor_capacity()));

    let (admitted, newer) = model_rejoin(&overlay, &removed, observer, departed);
    assert!(newer.generation() > retired.generation());
    assert!(admitted.nodes[&observer].topology.references(departed));

    let stale = enabled_step(
        &overlay,
        &admitted,
        observe(LifecycleEvent::Closed(retired)),
    );
    assert_eq!(
        stale.nodes[&observer].retirement_observable(),
        admitted.nodes[&observer].retirement_observable(),
        "the retired generation's close must be inert"
    );
    assert_eq!(stale.stale_effect, None);
}

/// Non-vacuity of `RetiredGenerationsAreInert`: with the exact-generation
/// guard removed, the search finds a minimal trace in which a retired
/// generation's close cancels the newer generation's handshake, and the
/// trace replays to the violation under the mutated shell only.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_removing_the_generation_guard_yields_a_replayable_counterexample() {
    let mutated = restart(ShellMutation::CallbackIgnoresGeneration);
    let trace = minimal_counterexample(
        &mutated,
        &restart(ShellMutation::Faithful),
        LawName::RetiredGenerationsAreInert,
    );
    let witness = replay(&mutated, &trace).stale_effect.unwrap();
    assert!(
        matches!(
            trace.last(),
            Some(OverlayAction::Observe { peer, event })
                if *peer == witness.peer && event.attempt() == witness.retired
        ),
        "the last step delivers the retired generation's event: {trace:#?}"
    );
}

/// Non-vacuity of `UnavailableHeadsAreReplaced`: with the unavailable head
/// removed but not replaced (`Preserve` where production selects
/// `Unavailable`), the search finds a minimal trace whose survivor keeps no
/// successor although sendable admitted peers remain.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_removing_the_replacement_invariant_yields_a_replayable_counterexample() {
    let mutated = departure(ShellMutation::ReplacementPreserves);
    let trace = minimal_counterexample(
        &mutated,
        &departure(ShellMutation::Faithful),
        LawName::UnavailableHeadsAreReplaced,
    );
    let departed = trace
        .iter()
        .find_map(|action| match action {
            OverlayAction::Depart(peer) => Some(*peer),
            _ => None,
        })
        .unwrap();
    let violated = replay(&mutated, &trace);
    assert!(
        violated.nodes.values().any(|node| {
            node.unreplaced_head == Some(UnreplacedHead { removed: departed })
                && node.topology.successors.is_empty()
        }),
        "the departed peer's head slot is left empty: {trace:#?}"
    );
}

/// Non-vacuity of `TopologyReferencesOnlyAdmitted`: with the registry
/// retiring a generation while the topology keeps its peer, the search finds
/// a minimal trace, ending in the retiring event, in which the ring
/// references a peer no admitted generation backs.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_retiring_without_removal_yields_a_replayable_counterexample() {
    let mutated = restart(ShellMutation::RetireWithoutRemove);
    let trace = minimal_counterexample(
        &mutated,
        &restart(ShellMutation::Faithful),
        LawName::TopologyReferencesOnlyAdmitted,
    );
    assert!(
        matches!(
            trace.last(),
            Some(OverlayAction::Observe {
                event: LifecycleEvent::RetireUnavailable(_) | LifecycleEvent::Closed(_),
                ..
            })
        ),
        "the last step retires a generation: {trace:#?}"
    );
    let violated = replay(&mutated, &trace);
    assert!(
        violated.nodes.values().any(|node| {
            node.topology
                .referenced_peers()
                .into_iter()
                .any(|peer| node.lifecycles.active_attempt(peer).is_none())
        }),
        "{trace:#?}"
    );
}

/// Non-vacuity of `StorageRepairFollowsVacatedSlots`: with every retirement
/// requesting a repair round, the search finds a minimal trace in which an
/// admitted peer that no slot references (the fourth peer of a `K = 1`
/// ring, whose fingers all resolve to the head) is retired and a placement
/// scan is requested for it; the trace replays to the violation under the
/// mutated shell only.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_requesting_repair_on_every_retirement_yields_a_replayable_counterexample() {
    let mutated = replacement(ShellMutation::RepairOnEveryRetirement);
    let faithful = replacement(ShellMutation::Faithful);
    let trace = minimal_counterexample(
        &mutated,
        &faithful,
        LawName::StorageRepairFollowsVacatedSlots,
    );
    let Some((OverlayAction::Observe { peer, event }, prefix)) = trace.split_last() else {
        panic!("the last step retires a generation: {trace:#?}");
    };
    let removed = event.attempt().peer();
    assert!(
        matches!(
            event,
            LifecycleEvent::Closed(_) | LifecycleEvent::RetireUnavailable(_)
        ),
        "the last step retires a generation: {trace:#?}"
    );
    let before = replay(&faithful, prefix);
    assert!(
        before.nodes[peer]
            .lifecycles
            .active_attempt(removed)
            .is_some()
            && !before.nodes[peer].topology.references(removed),
        "the retired peer is admitted but unreferenced before the step: {trace:#?}"
    );
    assert_eq!(
        replay(&mutated, &trace).nodes[peer].misdirected_repair,
        Some(MisdirectedRepair {
            removed,
            requested: true,
        })
    );
}

/// Law: `Remove{ReplaceWith(X)} = Remove{ReplaceWith(Successors(X, n, K))}`,
/// for a removed head and for a removed non-head (where both ignore `X`).
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
    let (head, tail) = peers.split_first().unwrap();
    for removed in peers.iter().copied() {
        for current in [vec![removed], vec![*head, removed], vec![*head, tail[0]]] {
            for mask in 0u32..(1 << peers.len()) {
                let candidates = peers
                    .iter()
                    .enumerate()
                    .filter(|(bit, peer)| mask & (1 << bit) != 0 && **peer != removed)
                    .map(|(_, peer)| *peer)
                    .collect::<Vec<_>>();
                let state =
                    TopologyState::new(*local, current.clone(), None, vec![None; FINGER_SLOTS]);
                let remove = |replacements| TopologyEvent::Remove {
                    peer: removed,
                    successor: SuccessorRemoval::ReplaceWith(replacements),
                };
                let normalized = successors(&candidates, *local, capacity);
                assert_eq!(
                    step(&state, remove(candidates), capacity),
                    step(&state, remove(normalized), capacity),
                );
            }
        }
    }
}

/// Non-vacuity of the liveness analysis and necessity of its premise: with
/// the premise dropped, a peer whose only successor closed is left without a
/// live head, and the analysis reports the starved suffix, which replays.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_without_the_live_head_premise_a_suffix_starves() {
    let overlay = departure(ShellMutation::Faithful);
    let report = check(&overlay, |_| true);
    let SearchReport::Safe {
        states,
        max_depth,
        liveness,
        ..
    } = report
    else {
        panic!("{report:#?}");
    };
    assert_eq!((states, max_depth), DEPARTURE_BOUNDS, "stale bounds table");
    let violation = liveness.violation.unwrap();
    assert!(
        violation
            .quiescent_suffix
            .iter()
            .all(|action| !action.is_environmental()),
        "{violation:#?}"
    );
    let trace = [violation.churn_prefix, violation.quiescent_suffix].concat();
    let starved = replay(&overlay, &trace);
    assert!(!laws::is_converged(&overlay, &starved), "{trace:#?}");
    assert!(!laws::retains_live_heads(&starved), "{trace:#?}");
}
