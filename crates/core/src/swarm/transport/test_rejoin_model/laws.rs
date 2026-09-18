//! The propositions checked over the composed carrier.
//!
//! Topology laws are quoted from [`TopologyState`](crate::dht::topology::TopologyState)
//! (`dht::topology::invariants`), where a churn simulator can import them; the
//! predicates here add only what needs the lifecycle registry or the
//! environment to state.
//!
//! Safety (`□`, every reachable state):
//! - `RetiredGenerationsAreInert`: no event of a generation `g` with
//!   `¬Owns(g)` changed `(topology, lifecycles)`.
//! - `TopologiesAreWellFormed`: `SuccessorsWellFormed ∧ PredecessorWellFormed
//!   ∧ FingersWellFormed` at every live peer.
//! - `RoutingAdvancesClockwise`: `RoutesClockwise(s, id)` for every live peer
//!   and every ring identity `id`.
//! - `TopologyReferencesOnlyAdmitted`: `Referenced(n, p) ⇒ Active(n, p)`.
//!
//! Coverage (`◇`, some reachable state), so the safety laws are not vacuous:
//! - `RetiredEventAwaitsBesideNewerGeneration`
//! - `SuccessorListIsTruncated`
//!
//! Liveness is stated in `search`, over [`is_converged`] and
//! [`retains_successor_paths`].

use std::collections::BTreeSet;

use super::overlay::Overlay;
use super::overlay::OverlayState;
use crate::dht::Did;

/// What a law claims about the reachable states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Expectation {
    /// `□`: every reachable state satisfies the predicate.
    Always,
    /// `◇`: some reachable state satisfies the predicate (coverage).
    Sometimes,
}

/// One checked proposition over the composed carrier.
#[derive(Clone, Copy)]
pub(super) struct Law {
    /// Name used in verdicts and by the mutation tests.
    pub(super) name: &'static str,
    /// Whether the predicate must hold everywhere or somewhere.
    pub(super) expectation: Expectation,
    /// The predicate.
    pub(super) holds: fn(&Overlay, &OverlayState) -> bool,
}

/// Name of the retired-generation law, shared with the mutation tests.
pub(super) const RETIRED_GENERATIONS_ARE_INERT: &str = "retired generations are inert";
/// Name of the successor-replacement law, shared with the mutation tests.
pub(super) const TOPOLOGY_REFERENCES_ONLY_ADMITTED: &str =
    "topology references only admitted generations";

/// `□ stale_effect = None`: the history variable never records a retired
/// generation's event changing protected state.
fn retired_generations_are_inert(_: &Overlay, state: &OverlayState) -> bool {
    state.stale_effect.is_none()
}

/// `□ ∀n. SuccessorsWellFormed(n, k) ∧ PredecessorWellFormed(n) ∧
/// FingersWellFormed(n)`.
fn topologies_are_well_formed(overlay: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().all(|node| {
        node.topology
            .successors_are_well_formed(overlay.successor_capacity())
            && node.topology.predecessor_is_well_formed()
            && node.topology.fingers_are_well_formed()
    })
}

/// `□ ∀n, id ∈ ring. RoutesClockwise(n, id)`.
fn routing_advances_clockwise(overlay: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().all(|node| {
        overlay
            .ring()
            .iter()
            .all(|target| node.topology.routes_clockwise_toward(*target))
    })
}

/// `□ ∀n, p. Referenced(n, p) ⇒ Active(n, p)`: every successor, predecessor,
/// and finger entry is backed by an admitted generation, so a removal or a
/// replacement never leaves evidence that no connection validates.
fn topology_references_only_admitted(_: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().all(|node| {
        node.topology
            .referenced_peers()
            .into_iter()
            .all(|peer| node.lifecycles.active_attempt(peer).is_some())
    })
}

/// `◇ ∃n, g, g'. Callback(g) ∈ callbacks(n) ∧ Active(n, g') ∧ g'.peer = g.peer
/// ∧ g' > g`: the rejoin race the model exists to explore is reachable, and
/// the next step delivers the retired event under the safety laws.
fn retired_event_awaits_beside_newer_generation(_: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().any(|node| {
        node.callbacks.iter().any(|callback| {
            let retired = callback.attempt();
            node.lifecycles
                .active_attempt(retired.peer)
                .is_some_and(|admitted| admitted.generation > retired.generation)
        })
    })
}

/// `◇ ∃n. |succ(n)| = k ∧ |Sendable(n)| > k`: more eligible peers than
/// successor capacity, so truncation and replacement choose among them.
fn successor_list_is_truncated(overlay: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().any(|node| {
        node.topology.successors.len() == overlay.successor_capacity()
            && node.lifecycles.active_connections().iter().count() > overlay.successor_capacity()
    })
}

/// `Converged(s)`: `∀n ∈ M. ChordFixpoint(n, M, k)` for the live set `M`.
pub(super) fn is_converged(overlay: &Overlay, state: &OverlayState) -> bool {
    let members = state.members();
    state.nodes.values().all(|node| {
        node.topology
            .is_chord_fixpoint_of(members.as_slice(), overlay.successor_capacity())
    })
}

/// `RetainsSuccessorPaths(s)`: in the digraph on the live set with an edge
/// `n → p` iff `p ∈ succ(n)` over a sendable generation whose link is alive,
/// every live peer reaches every live peer.
///
/// This is the premise of conditional liveness. It is a statement about the
/// physical overlay at the instant churn stops, so no later protocol step can
/// falsify it: the quiescent suffix kills no link.
pub(super) fn retains_successor_paths(state: &OverlayState) -> bool {
    state
        .nodes
        .keys()
        .all(|origin| reachable_over_successors(state, *origin).len() == state.nodes.len())
}

/// The peers reachable from `origin` over live successor edges, `origin`
/// included: the least fixpoint of one-step expansion.
fn reachable_over_successors(state: &OverlayState, origin: Did) -> BTreeSet<Did> {
    let mut reached = BTreeSet::from([origin]);
    let mut frontier = vec![origin];
    while let Some(peer) = frontier.pop() {
        let Some(node) = state.nodes.get(&peer) else {
            continue;
        };
        let live_successors = node
            .topology
            .successors
            .iter()
            .copied()
            .filter(|successor| {
                state.nodes.contains_key(successor)
                    && node
                        .lifecycles
                        .sendable_attempt(*successor)
                        .is_some_and(|under| state.is_linked(peer, under))
            });
        for successor in live_successors {
            if reached.insert(successor) {
                frontier.push(successor);
            }
        }
    }
    reached
}

/// Every checked proposition, in report order.
pub(super) fn laws() -> [Law; 6] {
    [
        Law {
            name: RETIRED_GENERATIONS_ARE_INERT,
            expectation: Expectation::Always,
            holds: retired_generations_are_inert,
        },
        Law {
            name: "topologies are well formed",
            expectation: Expectation::Always,
            holds: topologies_are_well_formed,
        },
        Law {
            name: "routing advances clockwise",
            expectation: Expectation::Always,
            holds: routing_advances_clockwise,
        },
        Law {
            name: TOPOLOGY_REFERENCES_ONLY_ADMITTED,
            expectation: Expectation::Always,
            holds: topology_references_only_admitted,
        },
        Law {
            name: "a retired generation's event awaits beside a newer admitted generation",
            expectation: Expectation::Sometimes,
            holds: retired_event_awaits_beside_newer_generation,
        },
        Law {
            name: "the successor list is truncated",
            expectation: Expectation::Sometimes,
            holds: successor_list_is_truncated,
        },
    ]
}
