//! Checked model of delivery toward a node DID (#873).
//!
//! State: an overlay `O = (V, L)` of per-node views `V : Did → TopologyState` and a symmetric
//! link relation `L ⊇ Known`, with links outside every view allowed (a joiner's bootstrap, a
//! leaf's guard). A payload for `T` at `n` in stage `σ` takes the production step
//! [`delivery_step`] with `linked = L(n)`.
//!
//! [`delivery_step`] depends on `(n, σ)` only, so a route is the orbit of a deterministic map on
//! the finite set `V × RouteStage`; a repeated state would be a cycle that only the hop budget
//! could end. The model records every route until it is delivered or ends, and asserts that no
//! state ever repeats.
//!
//! Laws checked on every route `n₀ → n₁ → …`:
//!
//! - `Linked`: every hop follows a link;
//! - `NeverOvershoots`: every hop that leaves the stage's handoff flag unset satisfies
//!   `next ∈ (n, aim]`;
//! - `SingleCrossing`: each aim is crossed at most once, by the hop that sets the flag;
//! - `Acyclic`: no `(node, stage)` repeats, so the hop budget never ends a route;
//! - `Bounded`: at most `2(|V| + 1) + 1` hops;
//! - `Exact`: an undelivered route toward `T` visited no node linked to `T`;
//! - `Converged ⇒ Delivered`, and the join-window law: on a converged ring without `J`, with
//!   `J` linked only to a bootstrap `B`, every member's report naming `reply_via = B` arrives.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use num_bigint::BigUint;
use rand::Rng;
use rand::SeedableRng;
use rand_hc::Hc128Rng;

use super::delivery_step;
use super::RouteStage;
use crate::dht::topology::dist;
use crate::dht::topology::find_successor;
use crate::dht::topology::finger_table;
use crate::dht::topology::successors;
use crate::dht::topology::FindSuccessorStep;
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
            route.states.len() <= 2 * (self.views.len() + 1) + 2,
            "unbounded: {route:?}"
        );
        let hops = route
            .states
            .iter()
            .copied()
            .zip(route.states.iter().copied().skip(1))
            .collect::<Vec<_>>();
        let mut crossings = BTreeMap::<Did, usize>::new();
        for ((from, before), (to, after)) in hops.iter().copied() {
            assert!(self.linked(from, to), "unlinked hop: {route:?}");
            if to == destination || to == after.aim(destination) {
                continue;
            }
            let aim = after.aim(destination);
            if after.handed_off && !before.handed_off {
                *crossings.entry(aim).or_default() += 1;
            } else {
                assert!(
                    dist(from, to) < dist(from, aim),
                    "overshoot toward {aim}: {route:?}"
                );
            }
        }
        assert!(
            crossings.values().all(|count| *count <= 1),
            "aim crossed twice: {route:?}"
        );
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

/// The Chord fixpoint view of `local` over `members`.
fn converged_view(members: &[Did], local: Did) -> TopologyState {
    view(local, members, finger_table(members, local))
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
/// forwards to the peer on `(n, aim]` nearest the aim over successors ∪ fingers and hands off
/// once to the head when none exists; a handoff receiver delivers over a link or ends the
/// route; a via peer hands over to the destination or ends the route.
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
    let toward = RouteStage::TOWARD;
    let handed = toward.handed_off();
    let via_local = RouteStage::replying_via(Some(local));
    let via_far = RouteStage::replying_via(Some(Did::from(50u32)));

    assert_eq!(step(30, toward, &[30]), Some((Did::from(30u32), toward)));
    assert_eq!(step(30, handed, &[30]), Some((Did::from(30u32), handed)));
    assert_eq!(step(30, toward, &[]), Some((Did::from(16u32), toward)));
    assert_eq!(step(50, toward, &[]), Some((Did::from(40u32), toward)));
    assert_eq!(step(4, toward, &[]), Some((Did::from(8u32), handed)));
    assert_eq!(step(4, handed, &[]), None);
    assert_eq!(step(4, via_local, &[4]), Some((Did::from(4u32), via_local)));
    assert_eq!(step(4, via_local, &[]), None);
    assert_eq!(step(4, via_far, &[]), Some((Did::from(40u32), via_far)));
    assert_eq!(step(4, via_far, &[50]), Some((Did::from(50u32), via_far)));
    assert_eq!(step(4, via_far.handed_off(), &[]), None);
    assert_eq!(
        delivery_step(
            &TopologyState::new(local, vec![], None, vec![]),
            Did::from(4u32),
            toward,
            |_| false,
        ),
        None
    );
}

/// #865 regression: the ring `A < B < T < C < D` with the successor lists of the trace. The
/// owner lookup still answers `C` for the position of `T` at `A`; the reply from its origin `D`
/// takes the `D – B` edge and arrives; from `A`, whose view does not know `T`'s neighbourhood,
/// the route fails fast at `C` instead of circling until the hop budget.
#[test]
fn test_issue_865_topology_delivers_reply_and_fails_fast() {
    let a = prefixed(0x0a33);
    let b = prefixed(0x15c8);
    let t = prefixed(0x31cf);
    let c = prefixed(0x91c0);
    let d = prefixed(0xaf3f);
    let views = [
        (a, vec![c, d]),
        (b, vec![t, c, d]),
        (t, vec![b]),
        (c, vec![d, a, b]),
        (d, vec![a, b, c]),
    ]
    .into_iter()
    .map(|(local, known)| (local, TopologyState::new(local, known, None, vec![None; 8])))
    .collect::<BTreeMap<_, _>>();
    let a_view = views.get(&a).cloned();
    let overlay = Overlay::new(views, []);

    assert_eq!(
        a_view.map(|view| find_successor(&view, t)),
        Some(FindSuccessorStep::Local(c))
    );
    assert_eq!(
        overlay.check_route_law(d, t, RouteStage::TOWARD).path(),
        vec![d, b, t]
    );
    let from_a = overlay.check_route_law(a, t, RouteStage::TOWARD);
    assert_eq!(from_a.path(), vec![a, c]);
    assert_eq!(from_a.outcome, Outcome::Unreachable);
    overlay.check_all_routes();
}

/// The join topologies of #873 §1.2: a report to a joiner linked only to its bootstrap arrives
/// when the request named `reply_via = bootstrap`, from every member; without the hint the
/// route ends with the typed error, never by the budget.
#[test]
fn test_join_window_report_arrives_through_reply_via() {
    for (members, bootstrap, joiner) in [
        ([0x1fd6, 0x26d3, 0x9114], 0x26d3, 0xeaab),
        ([0x1875, 0x9046, 0xe2c6], 0x9046, 0xf6f6),
    ] {
        let members = members.map(prefixed);
        let (bootstrap, joiner) = (prefixed(bootstrap), prefixed(joiner));
        let overlay = join_window(&members, joiner, bootstrap);
        for origin in members {
            let replied =
                overlay.check_route_law(origin, joiner, RouteStage::replying_via(Some(bootstrap)));
            assert_eq!(replied.outcome, Outcome::Delivered, "{replied:?}");
            let unhinted = overlay.check_route_law(origin, joiner, RouteStage::TOWARD);
            assert_ne!(unhinted.outcome, Outcome::Cycle, "{unhinted:?}");
        }
    }
}

/// Join-window law on rings of 2 to 6 random members: with the ring converged without the
/// joiner and the joiner linked only to a bootstrap at any position, every member's report
/// naming that bootstrap arrives.
#[test]
fn test_join_window_law() {
    let mut rng = Hc128Rng::seed_from_u64(873);
    for size in 2..=6usize {
        let everyone = random_members(&mut rng, size + 1);
        let (members, joiner) = everyone.split_at(size);
        let Some(joiner) = joiner.first().copied() else {
            continue;
        };
        for bootstrap in members.iter().copied() {
            let overlay = join_window(members, joiner, bootstrap);
            for origin in members.iter().copied() {
                let route = overlay.check_route_law(
                    origin,
                    joiner,
                    RouteStage::replying_via(Some(bootstrap)),
                );
                assert_eq!(route.outcome, Outcome::Delivered, "{route:?}");
            }
        }
    }
}

/// Exhaustive model: every assignment of known-peer sets on a 4-node ring, crossed with every
/// set of links to the destination that no view records and with every `reply_via` choice,
/// satisfies the route law.
#[test]
fn test_route_law_holds_on_every_four_node_view() {
    let members = [10u32, 20, 30, 40].map(Did::from);
    let subsets = |pool: &[Did]| {
        (0u32..(1 << pool.len()))
            .map(|mask| {
                pool.iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, peer)| *peer)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let choices = members.map(|local| {
        subsets(
            &members
                .iter()
                .copied()
                .filter(|peer| *peer != local)
                .collect::<Vec<_>>(),
        )
    });

    for assignment in 0usize..4096 {
        let views = members
            .iter()
            .zip(choices.iter())
            .enumerate()
            .map(|(index, (local, subsets))| {
                let choice = (assignment >> (3 * index)) & 0b111;
                let known = subsets.get(choice).cloned().unwrap_or_default();
                (*local, view(*local, &known, vec![]))
            })
            .collect::<BTreeMap<_, _>>();
        for destination in members {
            let others = members
                .iter()
                .copied()
                .filter(|peer| *peer != destination)
                .collect::<Vec<_>>();
            for linked in subsets(&others) {
                let extra = linked.iter().map(|peer| (*peer, destination));
                let overlay = Overlay::new(views.clone(), extra);
                let stages = std::iter::once(RouteStage::TOWARD).chain(
                    others
                        .iter()
                        .map(|peer| RouteStage::replying_via(Some(*peer))),
                );
                for stage in stages {
                    for origin in others.iter().copied() {
                        overlay.check_route_law(origin, destination, stage);
                    }
                }
            }
        }
    }

    let routes = converged(&members).check_all_routes();
    assert!(routes
        .iter()
        .all(|route| route.outcome == Outcome::Delivered));
}

/// Randomized model with a fixed seed: rings of 10 random DIDs with random successor
/// knowledge, finger hints and links outside every view satisfy the route law; the converged
/// overlay of the same members delivers every route.
#[test]
fn test_route_law_holds_on_random_unconverged_views() {
    let mut rng = Hc128Rng::seed_from_u64(865);
    for _ in 0..48 {
        let members = random_members(&mut rng, 10);
        let views = members
            .iter()
            .map(|local| {
                let known = members
                    .iter()
                    .copied()
                    .filter(|peer| peer != local && rng.gen_bool(0.3))
                    .collect::<Vec<_>>();
                let fingers = (0..8)
                    .map(|_| {
                        let pick = rng.gen_range(0..members.len());
                        members
                            .get(pick)
                            .copied()
                            .filter(|peer| peer != local && rng.gen_bool(0.3))
                    })
                    .collect();
                (*local, view(*local, &known, fingers))
            })
            .collect();
        let extra = members
            .iter()
            .flat_map(|a| members.iter().map(move |b| (*a, *b)))
            .filter(|_| rng.gen_bool(0.1))
            .collect::<Vec<_>>();
        Overlay::new(views, extra).check_all_routes();

        let routes = converged(&members).check_all_routes();
        assert!(routes
            .iter()
            .all(|route| route.outcome == Outcome::Delivered));
    }
}
