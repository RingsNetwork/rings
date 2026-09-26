//! Checked model of delivery toward a node DID (#873).
//!
//! State: an overlay `O = (V, L)` of per-node views `V : Did → TopologyState` and a symmetric
//! link relation `L`. Views may name peers they have no link to (a successor a stabilization
//! report introduced before its connection exists), and links may exist outside every view (a
//! joiner's bootstrap, a leaf's guard). A payload for `T` at `n` in stage `σ` takes the
//! production step [`delivery_step`] with `linked = L(n)`.
//!
//! [`delivery_step`] depends on `(n, σ)` only, so a route is the orbit of a deterministic map on
//! the finite set `V × RouteStage`; a repeated state would be a cycle that only the hop budget
//! could end. The model records every route until it is delivered or ends, and asserts that no
//! state ever repeats.
//!
//! The safety laws hold in every overlay and are checked on every route by
//! [`Overlay::check_route_law`]:
//!
//! - `Linked`: every hop follows a link;
//! - `NeverOvershoots`: every hop that does not set the stage's handoff flag satisfies
//!   `next ∈ (n, aim]` (crossing the aim only by that one flagged hop is then a property of the
//!   type: nothing clears the flag);
//! - `Acyclic`: no `(node, stage)` repeats, so the hop budget never ends a route;
//! - `Bounded`: at most `|V| + 2` hops (one greedy run, one handoff, two terminal deliveries);
//! - `Exact`: an undelivered route toward `T` visited no node linked to `T`.
//!
//! What a route achieves depends on the overlay, so the two regimes are tested apart:
//!
//! - [`converged`]: on the Chord fixpoint every route is delivered by greedy hops alone, each
//!   hop at least halves the remaining distance, and no node names a `reply_via`;
//! - [`unconverged`]: each kind of stale view has its own expected outcome: a stale successor
//!   is routed around, an unknown destination fails fast, a joiner's answers arrive through its
//!   `reply_via`, and unlinked successor entries are never hopped to.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use num_bigint::BigUint;
use rand::Rng;
use rand_hc::Hc128Rng;

use super::delivery_step;
use super::RouteStage;
use crate::dht::topology::dist;
use crate::dht::topology::finger_table;
use crate::dht::topology::predecessor;
use crate::dht::topology::successors;
use crate::dht::topology::TopologyState;
use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
use crate::dht::Did;

/// How a modelled route ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The payload reached its destination.
    Delivered,
    /// A hop ended the route with the typed error.
    Unreachable,
    /// A state repeated: a cycle only the hop budget could end.
    Cycle,
}

/// One modelled route: the `(node, stage)` states visited, origin first, and its end.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    /// `(n₀, σ₀) … (n_k, σ_k)`; a delivered route ends at the destination.
    states: Vec<(Did, RouteStage)>,
    /// The terminal outcome.
    outcome: Outcome,
}

impl Route {
    /// The nodes visited, origin first.
    fn path(&self) -> Vec<Did> {
        self.states.iter().map(|(node, _)| *node).collect()
    }
}

/// An overlay of per-node views and symmetric links.
struct Overlay {
    /// `V`: every node's local topology view.
    views: BTreeMap<Did, TopologyState>,
    /// `L`: unordered links, stored as `(min, max)`.
    links: BTreeSet<(Did, Did)>,
}

impl Overlay {
    /// The overlay whose links are the symmetric closure of every view's known peers plus
    /// `extra` links that no view records.
    fn new(
        views: BTreeMap<Did, TopologyState>,
        extra: impl IntoIterator<Item = (Did, Did)>,
    ) -> Self {
        let known = views.values().flat_map(|view| {
            view.successors
                .iter()
                .copied()
                .chain(view.fingers.iter().flatten().copied())
                .map(move |peer| (view.local, peer))
        });
        let links = known
            .chain(extra)
            .filter(|(a, b)| a != b)
            .map(|(a, b)| (a.min(b), a.max(b)))
            .collect();
        Self { views, links }
    }

    /// The overlay with exactly the links `links`, whatever the views name.
    fn with_links(
        views: BTreeMap<Did, TopologyState>,
        links: impl IntoIterator<Item = (Did, Did)>,
    ) -> Self {
        let links = links
            .into_iter()
            .filter(|(a, b)| a != b)
            .map(|(a, b)| (a.min(b), a.max(b)))
            .collect();
        Self { views, links }
    }

    /// `(a, b) ∈ L`.
    fn linked(&self, a: Did, b: Did) -> bool {
        self.links.contains(&(a.min(b), a.max(b)))
    }

    /// Route a payload from `origin` toward `destination`, starting in `stage`, under the
    /// production step until it is delivered, ends, or repeats a state.
    fn route(&self, origin: Did, destination: Did, stage: RouteStage) -> Route {
        let mut states = vec![(origin, stage)];
        let mut seen = HashSet::from([(origin, stage)]);
        let (mut at, mut stage) = (origin, stage);
        loop {
            if at == destination {
                return Route {
                    states,
                    outcome: Outcome::Delivered,
                };
            }
            let hop = self.views.get(&at).and_then(|view| {
                delivery_step(view, destination, stage, |peer| self.linked(at, peer))
            });
            let Some(hop) = hop else {
                return Route {
                    states,
                    outcome: Outcome::Unreachable,
                };
            };
            (at, stage) = (hop.peer, hop.stage);
            states.push((at, stage));
            if at != destination && !seen.insert((at, stage)) {
                return Route {
                    states,
                    outcome: Outcome::Cycle,
                };
            }
        }
    }

    /// Assert every route law for the route from `origin` to `destination` starting in
    /// `stage`, and return it for further assertions.
    fn check_route_law(&self, origin: Did, destination: Did, stage: RouteStage) -> Route {
        let route = self.route(origin, destination, stage);
        assert_ne!(route.outcome, Outcome::Cycle, "cycle: {route:?}");
        assert!(
            route.states.len() <= self.views.len() + 3,
            "unbounded: {route:?}"
        );
        let hops = route
            .states
            .iter()
            .copied()
            .zip(route.states.iter().copied().skip(1))
            .collect::<Vec<_>>();
        for ((from, before), (to, after)) in hops.iter().copied() {
            assert!(self.linked(from, to), "unlinked hop: {route:?}");
            let aim = after.aim(destination);
            let crossing = after.handed_off && !before.handed_off;
            if to != destination && to != aim && !crossing {
                assert!(
                    dist(from, to) < dist(from, aim),
                    "overshoot toward {aim}: {route:?}"
                );
            }
        }
        if route.outcome != Outcome::Delivered && stage.via.is_none() {
            assert!(
                route
                    .path()
                    .iter()
                    .all(|node| !self.linked(*node, destination)),
                "undelivered past a link to the destination: {route:?}"
            );
        }
        route
    }

    /// Check the route law toward every node from every other node.
    fn check_all_routes(&self) -> Vec<Route> {
        let nodes = self.views.keys().copied().collect::<Vec<_>>();
        nodes
            .iter()
            .flat_map(|origin| nodes.iter().map(move |destination| (*origin, *destination)))
            .filter(|(origin, destination)| origin != destination)
            .map(|(origin, destination)| {
                self.check_route_law(origin, destination, RouteStage::TOWARD)
            })
            .collect()
    }
}

/// A view of `local` knowing `known` as successors (sorted, capacity-bounded) and `fingers`
/// as its finger hints.
fn view(local: Did, known: &[Did], fingers: Vec<Option<Did>>) -> TopologyState {
    TopologyState::new(
        local,
        successors(known, local, DEFAULT_SUCCESSOR_CAPACITY),
        None,
        fingers,
    )
}

/// The Chord fixpoint view of `local` over `members`: successors, predecessor, and fingers.
fn converged_view(members: &[Did], local: Did) -> TopologyState {
    TopologyState::new(
        local,
        successors(members, local, DEFAULT_SUCCESSOR_CAPACITY),
        predecessor(members, local),
        finger_table(members, local),
    )
}

/// The converged overlay of `members`: fixpoint views, links = views.
fn converged(members: &[Did]) -> Overlay {
    let views = members
        .iter()
        .map(|local| (*local, converged_view(members, *local)))
        .collect();
    Overlay::new(views, [])
}

/// A converged ring of `members` plus a joiner `joiner` linked only to `bootstrap`.
fn join_window(members: &[Did], joiner: Did, bootstrap: Did) -> Overlay {
    let mut views = members
        .iter()
        .map(|local| (*local, converged_view(members, *local)))
        .collect::<BTreeMap<_, _>>();
    views.insert(joiner, view(joiner, &[bootstrap], vec![]));
    Overlay::new(views, [])
}

/// A DID whose top 16 bits are `prefix`, so fixtures read like the trace prefixes of #865.
fn prefixed(prefix: u32) -> Did {
    Did::from(BigUint::from(prefix) << 144u32)
}

/// `count` random DIDs drawn from `rng`.
fn random_members(rng: &mut Hc128Rng, count: usize) -> Vec<Did> {
    (0..count)
        .map(|_| {
            let mut bytes = [0u8; 20];
            rng.fill(&mut bytes);
            Did::from(BigUint::from_bytes_be(&bytes))
        })
        .collect()
}

/// Delivery step unit laws: a linked destination is delivered to in any stage; the greedy step
/// forwards to the linked known peer on `(n, aim]` nearest the aim over successors ∪ fingers,
/// skipping known peers without a link, and hands off once to the first linked known node when
/// none exists; a handoff receiver delivers over a link or ends the route; a via peer hands
/// over to the destination or ends the route.
#[test]
fn test_delivery_step_stages() {
    let local = Did::from(0u32);
    let current = TopologyState::new(local, vec![Did::from(8u32), Did::from(16u32)], None, vec![
        Some(Did::from(40u32)),
        None,
    ]);
    let step = |destination: u32, stage: RouteStage, linked: &[u32]| {
        delivery_step(&current, Did::from(destination), stage, |peer| {
            linked.iter().any(|linked| peer == Did::from(*linked))
        })
        .map(|hop| (hop.peer, hop.stage))
    };
    let view = [8, 16, 40];
    let toward = RouteStage::TOWARD;
    let handed = toward.handed_off();
    let via_local = RouteStage::replying_via(Some(local));
    let via_far = RouteStage::replying_via(Some(Did::from(50u32)));

    assert_eq!(step(30, toward, &[30]), Some((Did::from(30u32), toward)));
    assert_eq!(step(30, handed, &[30]), Some((Did::from(30u32), handed)));
    assert_eq!(step(30, toward, &view), Some((Did::from(16u32), toward)));
    assert_eq!(step(30, toward, &[8, 40]), Some((Did::from(8u32), toward)));
    assert_eq!(step(50, toward, &view), Some((Did::from(40u32), toward)));
    assert_eq!(step(4, toward, &view), Some((Did::from(8u32), handed)));
    assert_eq!(step(4, toward, &[16, 40]), Some((Did::from(16u32), handed)));
    assert_eq!(step(4, toward, &[]), None);
    assert_eq!(step(4, handed, &view), None);
    assert_eq!(step(4, via_local, &[4]), Some((Did::from(4u32), via_local)));
    assert_eq!(step(4, via_local, &view), None);
    assert_eq!(step(4, via_far, &view), Some((Did::from(40u32), via_far)));
    assert_eq!(step(4, via_far, &[50]), Some((Did::from(50u32), via_far)));
    assert_eq!(step(4, via_far.handed_off(), &view), None);
}

/// Laws of delivery on the Chord fixpoint.
mod converged;
/// Expected outcomes of delivery on each kind of stale view.
mod unconverged;
