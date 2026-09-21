//! The propositions checked over the composed carrier.
//!
//! Topology laws are quoted from [`TopologyState`](crate::dht::topology::TopologyState)
//! (`dht::topology::invariants`), where a churn simulator can import them; the
//! predicates here add only what needs the lifecycle registry or the
//! environment to state.
//!
//! Each law is named by a [`LawName`] variant, whose doc states it; this
//! header only sorts them. Safety (`□`): `RetiredGenerationsAreInert`,
//! `UnavailableHeadsAreReplaced`, `TopologyReferencesOnlyAdmitted`,
//! `TopologiesAreWellFormed`. Coverage (`◇`), so the safety laws are not
//! vacuous: `RetiredEventAwaitsBesideNewerGeneration`,
//! `HeadReplacementFillsCapacity`.
//!
//! `RoutesClockwise` is not listed: it is the unconditional postcondition of
//! `find_successor`, true of every representable state, so no reachable
//! state could falsify it.
//!
//! Liveness is stated in `search`, over [`is_converged`] and
//! [`retains_live_heads`].

use std::fmt;

use super::node::LifecycleEvent;
use super::overlay::Overlay;
use super::overlay::OverlayState;
use crate::dht::topology::successor_head;

/// What a law claims about the reachable states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Expectation {
    /// `□`: every reachable state satisfies the predicate.
    Always,
    /// `◇`: some reachable state satisfies the predicate (coverage).
    Sometimes,
}

/// The identity of a law, as verdicts and mutation tests refer to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LawName {
    /// `□`: a retired generation's event changes nothing protected.
    RetiredGenerationsAreInert,
    /// `□`: an `Unavailable` retirement of the head leaves
    /// `succ = Successors(Sendable ∖ {head}, n, K)`. Under the faithful shell
    /// this reduces to the normalization law of `step`'s `Remove`; what it
    /// witnesses is the shell's choice of removal flavour
    /// (`ReplacementPreserves`), while production's candidate computation is
    /// compared in `conformance`.
    UnavailableHeadsAreReplaced,
    /// `□`: `Referenced(n, p) ⇒ Active(n, p)`, topology evidence is backed
    /// by an admitted generation.
    TopologyReferencesOnlyAdmitted,
    /// `□`: `WellFormed(topology[n], K)` at every live peer. Every topology
    /// write of the shell goes through `step`, which normalizes its
    /// arguments, so this law is falsifiable only by a defect of `step`
    /// itself (the subject of the topology unit tests); it is checked so the
    /// composition inherits the invariant it relies on.
    TopologiesAreWellFormed,
    /// `◇`: a retired generation's event awaits beside a newer admitted
    /// generation of the same peer, so the next step delivers it under the
    /// safety laws.
    RetiredEventAwaitsBesideNewerGeneration,
    /// `◇`: the head's `RetireUnavailable` is pending while at least `K`
    /// other sendable peers remain, so the replacement chooses a full list;
    /// with more than `K` (the `replacement` configuration) it truncates.
    HeadReplacementFillsCapacity,
}

impl fmt::Display for LawName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RetiredGenerationsAreInert => "retired generations are inert",
            Self::UnavailableHeadsAreReplaced => {
                "an unavailable head is replaced by the sendable admitted successors"
            }
            Self::TopologyReferencesOnlyAdmitted => "topology references only admitted generations",
            Self::TopologiesAreWellFormed => "topologies are well formed",
            Self::RetiredEventAwaitsBesideNewerGeneration => {
                "a retired generation's event awaits beside a newer admitted generation"
            }
            Self::HeadReplacementFillsCapacity => {
                "a head replacement has at least as many sendable candidates as capacity"
            }
        })
    }
}

/// One checked proposition over the composed carrier.
#[derive(Clone, Copy)]
pub(super) struct Law {
    /// Identity used in verdicts and by the mutation tests.
    pub(super) name: LawName,
    /// Whether the predicate must hold everywhere or somewhere.
    pub(super) expectation: Expectation,
    /// The predicate.
    pub(super) holds: fn(&Overlay, &OverlayState) -> bool,
}

/// `□ stale_effect = None`: the history variable never records a retired
/// generation's event changing protected state.
fn retired_generations_are_inert(_: &Overlay, state: &OverlayState) -> bool {
    state.stale_effect.is_none()
}

/// `□ ∀n. unreplaced_head(n) = None`: the history variable never records an
/// unavailable head retired without the sendable admitted successors taking
/// its place.
fn unavailable_heads_are_replaced(_: &Overlay, state: &OverlayState) -> bool {
    state
        .nodes
        .values()
        .all(|node| node.unreplaced_head.is_none())
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

/// `□ ∀n. WellFormed(topology[n], K)`.
fn topologies_are_well_formed(overlay: &Overlay, state: &OverlayState) -> bool {
    state
        .nodes
        .values()
        .all(|node| node.topology.is_well_formed(overlay.successor_capacity()))
}

/// `◇ ∃n, g, g'. Event(g) ∈ events(n) ∧ Active(n, g') ∧ g'.peer = g.peer ∧
/// g' > g`: the rejoin race the model exists to explore is reachable, and the
/// next step delivers the retired event under the safety laws.
fn retired_event_awaits_beside_newer_generation(_: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().any(|node| {
        node.events.iter().any(|event| {
            let retired = event.attempt();
            node.lifecycles
                .active_attempt(retired.peer())
                .is_some_and(|admitted| admitted.generation() > retired.generation())
        })
    })
}

/// `◇ ∃n, g. RetireUnavailable(g) ∈ events(n) ∧ g.peer = head(n) ∧
/// |Sendable(n) ∖ {g.peer}| ≥ K`: the next step replaces a head from enough
/// candidates to fill the list, so `UnavailableHeadsAreReplaced` is checked
/// on a replacement that chooses (and, above `K`, truncates).
fn head_replacement_fills_capacity(overlay: &Overlay, state: &OverlayState) -> bool {
    state.nodes.values().any(|node| {
        node.events.iter().any(|event| match event {
            LifecycleEvent::RetireUnavailable(head) => {
                successor_head(&node.topology) == Some(head.peer())
                    && node
                        .lifecycles
                        .active_connections()
                        .iter()
                        .filter(|candidate| candidate.peer() != head.peer())
                        .count()
                        >= overlay.successor_capacity()
            }
            LifecycleEvent::ChannelOpened(_)
            | LifecycleEvent::SendTerminal(_)
            | LifecycleEvent::Closed(_) => false,
        })
    })
}

/// `Converged(s)`: `∀n ∈ M. ChordFixpoint(n, M, K)` for the live set `M`.
pub(super) fn is_converged(overlay: &Overlay, state: &OverlayState) -> bool {
    let members = state.members();
    state.nodes.values().all(|node| {
        node.topology
            .is_chord_fixpoint_of(members.as_slice(), overlay.successor_capacity())
    })
}

/// `RetainsLiveHeads(s)`: every live peer's successor head is live, sendable,
/// and its link is alive.
///
/// This is the premise of conditional liveness, evaluated at the state where
/// churn stops. It is the weakest statement of "the remaining overlay keeps
/// a reachable successor path" this model can make: a peer whose head is
/// gone has no successor edge at all (the #775 orphan), and a peer whose
/// head is live but stale is repaired by stabilization. It is a statement
/// about that instant only; a later `Stabilize` may swap the head for a
/// confirmed peer whose link is already dead, and the claim covers those
/// behaviours too.
pub(super) fn retains_live_heads(state: &OverlayState) -> bool {
    state.nodes.iter().all(|(peer, node)| {
        successor_head(&node.topology).is_some_and(|head| {
            state.nodes.contains_key(&head)
                && node
                    .lifecycles
                    .sendable_attempt(head)
                    .is_some_and(|under| state.far_end_of(*peer, under).is_some())
        })
    })
}

/// Every checked proposition, in report order.
pub(super) const LAWS: [Law; 6] = [
    Law {
        name: LawName::RetiredGenerationsAreInert,
        expectation: Expectation::Always,
        holds: retired_generations_are_inert,
    },
    Law {
        name: LawName::UnavailableHeadsAreReplaced,
        expectation: Expectation::Always,
        holds: unavailable_heads_are_replaced,
    },
    Law {
        name: LawName::TopologyReferencesOnlyAdmitted,
        expectation: Expectation::Always,
        holds: topology_references_only_admitted,
    },
    Law {
        name: LawName::TopologiesAreWellFormed,
        expectation: Expectation::Always,
        holds: topologies_are_well_formed,
    },
    Law {
        name: LawName::RetiredEventAwaitsBesideNewerGeneration,
        expectation: Expectation::Sometimes,
        holds: retired_event_awaits_beside_newer_generation,
    },
    Law {
        name: LawName::HeadReplacementFillsCapacity,
        expectation: Expectation::Sometimes,
        holds: head_replacement_fills_capacity,
    },
];
