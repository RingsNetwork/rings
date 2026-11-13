//! Delivery on stale views: each kind of staleness has its own expected outcome.
//!
//! | Staleness | Expected outcome |
//! |---|---|
//! | a successor list skips `T`, but some view links toward `T` | routed around, delivered |
//! | a sender's only link is past the destination (leaf behind its guard, joiner behind its bootstrap) | one handoff, then the receiver routes on; delivered |
//! | only `T`'s predecessor is linked to `T` | unlinked entries skipped; delivered at the predecessor |
//! | no node is linked to `T` | fails fast with the typed error within `2|V|` hops |
//! | `T` has no predecessor and is linked only to its bootstrap `B` | its answers arrive through `reply_via = B` |
//! | successor entries name peers without a link | never hopped to; delivered over the linked ring |
//! | arbitrary views and links | the safety laws of [`Overlay::check_route_law`] only |

use std::collections::BTreeMap;

use rand::Rng;
use rand::SeedableRng;
use rand_hc::Hc128Rng;

use super::converged_view;
use super::join_window;
use super::prefixed;
use super::random_members;
use super::view;
use super::Outcome;
use super::Overlay;
use super::RouteStage;
use crate::dht::delivery::origination;
use crate::dht::topology::find_successor;
use crate::dht::topology::predecessor;
use crate::dht::topology::successors;
use crate::dht::topology::FindSuccessorStep;
use crate::dht::topology::TopologyState;
use crate::dht::Did;

/// The #865 overlay: the ring `A < B < T < C < D` with the successor lists of the trace, and
/// its five members in ring order.
fn issue_865() -> (Overlay, [Did; 5]) {
    let [a, b, t, c, d] = [0x0a33, 0x15c8, 0x31cf, 0x91c0, 0xaf3f].map(prefixed);
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
    (Overlay::new(views, []), [a, b, t, c, d])
}

/// Stale successor, routed around (#865): `A`'s successor list skips `B` and `T`, yet the reply
/// from `D` takes the `D – B` edge, and from `C` the `C – B` edge. The owner lookup at `A` is
/// unchanged and still answers `C` for `T`'s position.
#[test]
fn test_stale_successor_is_routed_around_through_a_linked_neighbour() {
    let (overlay, [a, b, t, c, d]) = issue_865();

    assert_eq!(
        overlay.views.get(&a).map(|view| find_successor(view, t)),
        Some(FindSuccessorStep::Local(c))
    );
    let from_d = overlay.check_route_law(d, t, RouteStage::TOWARD);
    assert_eq!(
        (from_d.path(), from_d.outcome),
        (vec![d, b, t], Outcome::Delivered)
    );
    let from_c = overlay.check_route_law(c, t, RouteStage::TOWARD);
    assert_eq!(
        (from_c.path(), from_c.outcome),
        (vec![c, b, t], Outcome::Delivered)
    );
    overlay.check_all_routes();
}

/// One crossing suffices (#865): from `A`, whose view knows nothing on the way to `T`, the route
/// hands off once, to `C`, whose fuller view routes on through `B`; the owner-lookup route
/// circled `A → C → D → A` until the hop budget.
#[test]
fn test_stale_sender_hands_off_once_and_the_receiver_routes_on() {
    let (overlay, [a, b, t, c, _]) = issue_865();

    let from_a = overlay.check_route_law(a, t, RouteStage::TOWARD);
    assert_eq!(from_a.path(), vec![a, c, b, t]);
    assert_eq!(from_a.outcome, Outcome::Delivered);
    assert_eq!(
        from_a.states.get(1).map(|(_, stage)| *stage),
        Some(RouteStage::TOWARD.handed_off())
    );
}

/// A node without links fails fast: on a converged ring whose members all name `T` in their
/// views but none is linked to it, every route toward `T` ends with the typed error, never by
/// the hop budget, within the `2|V|` bound the route law checks.
#[test]
fn test_destination_without_links_fails_fast() {
    let mut rng = Hc128Rng::seed_from_u64(5);
    for size in [2usize, 3, 5, 8] {
        let everyone = random_members(&mut rng, size + 1);
        let (members, offline) = everyone.split_at(size);
        let Some(offline) = offline.first().copied() else {
            continue;
        };
        let views = members
            .iter()
            .map(|local| (*local, converged_view(&everyone, *local)))
            .collect::<BTreeMap<_, _>>();
        let links = members
            .iter()
            .flat_map(|a| members.iter().map(move |b| (*a, *b)));
        let overlay = Overlay::with_links(views, links);
        for origin in members.iter().copied() {
            let route = overlay.check_route_law(origin, offline, RouteStage::TOWARD);
            assert_eq!(route.outcome, Outcome::Unreachable, "{route:?}");
        }
    }
}

/// A leaf behind its predecessor is reached by every member: every member names the leaf in
/// its view, but only the leaf's ring predecessor is linked to it, so greedy delivery skips the
/// unlinked entries and hands the payload over at the predecessor.
#[test]
fn test_leaf_behind_its_predecessor_is_reached_by_every_member() {
    let mut rng = Hc128Rng::seed_from_u64(6);
    for size in [2usize, 3, 5, 8] {
        let everyone = random_members(&mut rng, size + 1);
        let (members, leaf) = everyone.split_at(size);
        let Some(leaf) = leaf.first().copied() else {
            continue;
        };
        let Some(guard) = predecessor(&everyone, leaf) else {
            continue;
        };
        let views = everyone
            .iter()
            .map(|local| (*local, converged_view(&everyone, *local)))
            .collect::<BTreeMap<_, _>>();
        let links = members
            .iter()
            .flat_map(|a| members.iter().map(move |b| (*a, *b)))
            .chain(std::iter::once((guard, leaf)));
        let overlay = Overlay::with_links(views, links);
        for origin in members.iter().copied() {
            let route = overlay.check_route_law(origin, leaf, RouteStage::TOWARD);
            assert_eq!(route.outcome, Outcome::Delivered, "{route:?}");
        }
    }
}

/// A joiner, answered through `reply_via` (#873 §1.2): the joiner has no predecessor, so its
/// requests name its bootstrap, and every member's answer to it arrives. Without the hint an
/// answer arrives only if one of its two greedy runs happens to pass the bootstrap; in these two
/// topologies neither does, so the unhinted route fails fast: the hint is what makes delivery
/// certain.
#[test]
fn test_joiner_answers_arrive_through_reply_via() {
    for (members, bootstrap, joiner) in [
        ([0x1fd6, 0x26d3, 0x9114], 0x26d3, 0xeaab),
        ([0x1875, 0x9046, 0xe2c6], 0x9046, 0xf6f6),
    ] {
        let members = members.map(prefixed);
        let (bootstrap, joiner) = (prefixed(bootstrap), prefixed(joiner));
        let overlay = join_window(&members, joiner, bootstrap);
        let named = overlay
            .views
            .get(&joiner)
            .and_then(|view| origination(view, bootstrap, |peer| peer == bootstrap).reply_via);
        assert_eq!(named, Some(bootstrap));
        for origin in members {
            let replied = overlay.check_route_law(origin, joiner, RouteStage::replying_via(named));
            assert_eq!(replied.outcome, Outcome::Delivered, "{replied:?}");
            if origin != bootstrap {
                let unhinted = overlay.check_route_law(origin, joiner, RouteStage::TOWARD);
                assert_eq!(unhinted.outcome, Outcome::Unreachable, "{unhinted:?}");
            }
        }
    }
}

/// Join-window law on fixed-seed random rings of 2 to 6 members: with the ring converged
/// without the joiner and the joiner linked only to a bootstrap at any position, every
/// member's answer naming that bootstrap arrives.
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

/// A sparse sender reaches every member: a node linked only to one member `B` of a ring
/// converged without it, at any position of `B` (a leaf behind its guard, which is its ring
/// predecessor, or a joiner behind an arbitrary bootstrap), delivers to every member. Its only
/// hop is past most destinations, and `B`, whose view is complete, must still route on.
#[test]
fn test_sparse_sender_reaches_every_member() {
    let mut rng = Hc128Rng::seed_from_u64(4);
    for size in [2usize, 3, 5, 8] {
        let everyone = random_members(&mut rng, size + 1);
        let (members, sender) = everyone.split_at(size);
        let Some(sender) = sender.first().copied() else {
            continue;
        };
        for anchor in members.iter().copied() {
            let overlay = join_window(members, sender, anchor);
            for destination in members.iter().copied() {
                let route = overlay.check_route_law(sender, destination, RouteStage::TOWARD);
                assert_eq!(route.outcome, Outcome::Delivered, "{route:?}");
            }
        }
    }
}

/// Unlinked successor entries are never hopped to: every view holds the full converged
/// knowledge, but only the ring edges `n – succ(n)` are linked. Every route is still delivered,
/// over linked hops only (`Linked` is part of the route law).
#[test]
fn test_unlinked_view_peers_are_skipped() {
    let mut rng = Hc128Rng::seed_from_u64(3);
    for size in [3usize, 5, 8] {
        let members = random_members(&mut rng, size);
        let views = members
            .iter()
            .map(|local| (*local, converged_view(&members, *local)))
            .collect::<BTreeMap<_, _>>();
        let ring = members.iter().filter_map(|local| {
            successors(&members, *local, 1)
                .first()
                .map(|next| (*local, *next))
        });
        let overlay = Overlay::with_links(views, ring);
        for route in overlay.check_all_routes() {
            assert_eq!(route.outcome, Outcome::Delivered, "{route:?}");
        }
    }
}

/// Arbitrary views, exhaustively: every assignment of known-peer sets on a 4-node ring, crossed
/// with every set of links to the destination that no view records and with every `reply_via`
/// choice, satisfies the safety laws.
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
}

/// Arbitrary views, randomized with a fixed seed: rings of 10 random DIDs with random
/// successor knowledge and finger hints, and links drawn independently of the views (views name
/// unlinked peers, links exist outside views), satisfy the safety laws.
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
        let links = members
            .iter()
            .flat_map(|a| members.iter().map(move |b| (*a, *b)))
            .filter(|_| rng.gen_bool(0.3))
            .collect::<Vec<_>>();
        Overlay::with_links(views, links).check_all_routes();
    }
}

/// Non-transitive two-clique ring (`#1195`): cliques `V₁ = {A(10), B(30), C(50)}` and
/// `V₂ = {C(50), D(80), E(40)}` share only articulation node `C(50)` with interleaved ring
/// order `A(10) < B(30) < E(40) < C(50) < D(80)`. With bounded multi-predecessor retention at
/// `C(50)` and passive 1-hop bridge hints learned from `C(50)`'s `TopoInfo`, every ordered pair
/// delivers in at most 2 hops and satisfies all route safety laws.
#[test]
fn test_non_transitive_two_clique_ring_delivers_all_pairs_and_satisfies_route_laws() {
    let [a, b, e, c, d] = [10u32, 30, 40, 50, 80].map(Did::from);
    let clique_v1 = [a, b, c];
    let clique_v2 = [c, d, e];

    let mut view_a = converged_view(&clique_v1, a);
    view_a.bridged.insert(d, c);
    view_a.bridged.insert(e, c);

    let mut view_b = converged_view(&clique_v1, b);
    view_b.bridged.insert(d, c);
    view_b.bridged.insert(e, c);

    let mut view_c = TopologyState::new(
        c,
        vec![d, a, b],
        Some(e),
        crate::dht::topology::finger_table(&[a, b, c, d, e], c),
    );
    view_c.predecessors = vec![e, b];

    let mut view_d = converged_view(&clique_v2, d);
    view_d.bridged.insert(a, c);
    view_d.bridged.insert(b, c);

    let mut view_e = converged_view(&clique_v2, e);
    view_e.bridged.insert(a, c);
    view_e.bridged.insert(b, c);

    let views = BTreeMap::from([
        (a, view_a),
        (b, view_b),
        (c, view_c),
        (d, view_d),
        (e, view_e),
    ]);
    let links = [(a, b), (b, c), (a, c), (c, d), (d, e), (c, e)];
    let overlay = Overlay::with_links(views, links);

    for route in overlay.check_all_routes() {
        assert_eq!(route.outcome, Outcome::Delivered, "{route:?}");
        assert!(
            route.path().len() <= 4,
            "expected at most 3 hops: {route:?}"
        );
    }

    let e_to_a = overlay.check_route_law(e, a, RouteStage::TOWARD);
    assert_eq!(
        (e_to_a.path(), e_to_a.outcome),
        (vec![e, d, c, a], Outcome::Delivered)
    );

    let d_to_b = overlay.check_route_law(d, b, RouteStage::TOWARD);
    assert_eq!(
        (d_to_b.path(), d_to_b.outcome),
        (vec![d, c, b], Outcome::Delivered)
    );

    let a_to_e = overlay.check_route_law(a, e, RouteStage::TOWARD);
    assert_eq!(
        (a_to_e.path(), a_to_e.outcome),
        (vec![a, b, c, e], Outcome::Delivered)
    );

    let b_to_d = overlay.check_route_law(b, d, RouteStage::TOWARD);
    assert_eq!(
        (b_to_d.path(), b_to_d.outcome),
        (vec![b, c, d], Outcome::Delivered)
    );
}

/// Stale bridge hint fallback (`#1195` P1 regression): in two cliques `{A10, B20, C30}` and
/// `{C30, D40, E50}`, `A10` holds `bridged[E50] = C30` from non-head successor sync. If only
/// the direct `C30–E50` link subsequently fails while `E50` remains reachable through
/// `C30–D40–E50`, `A10` routes to `C30` without pinning `via = Some(C30)`, so `C30` continues
/// along `C30 -> D40 -> E50` instead of dropping the payload.
#[test]
fn test_stale_bridge_hint_falls_back_through_remaining_subclique_links() {
    let [a, b, c, d, e] = [10u32, 20, 30, 40, 50].map(Did::from);
    let clique_v1 = [a, b, c];
    let clique_v2 = [c, d, e];

    let mut view_a = converged_view(&clique_v1, a);
    view_a.bridged.insert(d, c);
    view_a.bridged.insert(e, c);

    let view_b = converged_view(&clique_v1, b);
    let view_c = TopologyState::new(
        c,
        vec![d, a, b],
        Some(b),
        crate::dht::topology::finger_table(&[a, b, c, d], c),
    );
    let view_d = converged_view(&clique_v2, d);
    let view_e = converged_view(&clique_v2, e);

    let views = BTreeMap::from([
        (a, view_a),
        (b, view_b),
        (c, view_c),
        (d, view_d),
        (e, view_e),
    ]);
    // Link `C–E` is down, while `C–D` and `D–E` remain up.
    let links = [(a, b), (b, c), (a, c), (c, d), (d, e)];
    let overlay = Overlay::with_links(views, links);

    let a_to_e = overlay.check_route_law(a, e, RouteStage::TOWARD);
    assert_eq!(
        (a_to_e.path(), a_to_e.outcome),
        (vec![a, c, d, e], Outcome::Delivered)
    );
}

/// Gap-bridge handoff destination guard (`#1195` P2 regression): an unrelated bridged peer
/// closer than the destination (`dist(local, bridged) < dist(local, aim)`) must not hijack a
/// viable normal handoff.
#[test]
fn test_gap_bridge_handoff_ignores_unrelated_bridges_before_destination() {
    let scale = |id: u32| prefixed(id << 8);
    let [n10, n30, n60, n100, n160, n180, n190, n210, n230] =
        [10u32, 30, 60, 100, 160, 180, 190, 210, 230].map(scale);

    let mut view_10 = TopologyState::new(n10, vec![n160, n210], None, vec![None; 160]);
    view_10.bridged.insert(n30, n210);
    view_10.bridged.insert(n100, n160);
    view_10.bridged.insert(n180, n160);
    view_10.bridged.insert(n190, n160);
    view_10.bridged.insert(n230, n160);

    let clique = [n60, n100, n160, n180, n190, n230];
    let mut view_160 = converged_view(&clique, n160);
    view_160.predecessors = vec![n100, n10];

    let view_210 = TopologyState::new(n210, vec![n30, n10], Some(n10), vec![None; 160]);
    let view_30 = TopologyState::new(n30, vec![n210], Some(n210), vec![None; 160]);
    let view_60 = converged_view(&clique, n60);

    let views = BTreeMap::from([
        (n10, view_10),
        (n30, view_30),
        (n60, view_60),
        (n160, view_160),
        (n210, view_210),
    ]);
    let links = [
        (n10, n160),
        (n10, n210),
        (n30, n210),
        (n160, n60),
        (n160, n100),
        (n160, n180),
        (n160, n190),
        (n160, n230),
    ];
    let overlay = Overlay::with_links(views, links);

    let route = overlay.check_route_law(n10, n60, RouteStage::TOWARD);
    assert_eq!(
        (route.path(), route.outcome),
        (vec![n10, n160, n60], Outcome::Delivered)
    );
}

/// Regression (`#1221` Item 1): when `route_toward` returns `RouteStep::Handoff(peer)`, an
/// intermediate `bridged_peer != aim` with `dist(local, aim) < dist(local, bridged_peer) <
/// dist(local, peer)` must NOT hijack the handoff to `via`, because at `via` forwarding to
/// `bridged_peer` would cross `aim` a second time and be dropped (`stage.handed_off == true`).
#[test]
fn test_gap_bridge_handoff_ignores_intermediate_bridged_peer_past_aim() {
    let [n10, aim20, bridged30, head50, via60] = [10u32, 20, 30, 50, 60].map(Did::from);

    let mut view_10 = TopologyState::new(n10, vec![head50, via60], None, vec![None; 8]);
    // `bridged30 != aim20` lies strictly between `aim20` and `head50`, reported by `via60`.
    // `via60` is linked to `bridged30`, NOT `aim20`, while `head50` IS linked to `aim20`.
    view_10.bridged.insert(bridged30, via60);

    let view_50 = TopologyState::new(head50, vec![via60, n10], Some(aim20), vec![None; 8]);
    let view_60 = TopologyState::new(via60, vec![n10, bridged30], Some(head50), vec![None; 8]);
    let view_30 = TopologyState::new(bridged30, vec![head50], Some(aim20), vec![None; 8]);
    let view_20 = TopologyState::new(aim20, vec![head50], Some(n10), vec![None; 8]);

    let views = BTreeMap::from([
        (n10, view_10),
        (aim20, view_20),
        (bridged30, view_30),
        (head50, view_50),
        (via60, view_60),
    ]);
    let links = [
        (n10, head50),
        (n10, via60),
        (head50, aim20),
        (via60, bridged30),
        (bridged30, aim20),
    ];
    let overlay = Overlay::with_links(views, links);

    let route = overlay.check_route_law(n10, aim20, RouteStage::TOWARD);
    assert_eq!(
        (route.path(), route.outcome),
        (vec![n10, head50, aim20], Outcome::Delivered)
    );
}

/// Non-transitive two-clique topology `(ABC)(CDE)` where `A, B` cannot link directly to `D, E`
/// and `C` bridges both cliques (`K = 4` successors at `C`): all cross-clique pairs `(A -> E)`,
/// `(B -> D)`, `(E -> A)`, `(D -> B)` plus `RouteStage::via` replies route cleanly through `C`,
/// and `find_successor(state, state.local)` resolves locally.
#[test]
fn test_non_transitive_two_clique_bridge_routes_both_directions_and_reply_via() {
    let [a10, b20, c30, d40, e50] = [10u32, 20, 30, 40, 50].map(Did::from);

    let mut view_a = TopologyState::new(a10, vec![b20, c30], Some(c30), vec![None; 8]);
    view_a.bridged.insert(d40, c30);
    view_a.bridged.insert(e50, c30);

    let mut view_b = TopologyState::new(b20, vec![c30, a10], Some(a10), vec![None; 8]);
    view_b.bridged.insert(d40, c30);
    view_b.bridged.insert(e50, c30);

    let mut view_c = TopologyState::new(c30, vec![d40, e50, a10, b20], Some(b20), vec![None; 8]);
    view_c.predecessors = vec![b20, e50];

    let mut view_d = TopologyState::new(d40, vec![e50, c30], Some(c30), vec![None; 8]);
    view_d.bridged.insert(a10, c30);
    view_d.bridged.insert(b20, c30);

    let mut view_e = TopologyState::new(e50, vec![c30, d40], Some(d40), vec![None; 8]);
    view_e.bridged.insert(a10, c30);
    view_e.bridged.insert(b20, c30);

    assert_eq!(
        crate::dht::topology::find_successor(&view_a, a10),
        crate::dht::topology::FindSuccessorStep::Local(a10)
    );

    let views = BTreeMap::from([
        (a10, view_a),
        (b20, view_b),
        (c30, view_c),
        (d40, view_d),
        (e50, view_e),
    ]);
    let links = [
        (a10, b20),
        (b20, c30),
        (a10, c30),
        (c30, d40),
        (d40, e50),
        (c30, e50),
    ];
    let overlay = Overlay::with_links(views, links);

    for (src, dst, expected) in [
        (a10, e50, vec![a10, c30, e50]),
        (b20, d40, vec![b20, c30, d40]),
        (e50, a10, vec![e50, c30, a10]),
        (d40, b20, vec![d40, e50, c30, b20]),
    ] {
        let route = overlay.check_route_law(src, dst, RouteStage::TOWARD);
        assert_eq!(
            (route.path(), route.outcome),
            (expected, Outcome::Delivered)
        );
    }

    let reply = overlay.check_route_law(e50, a10, RouteStage::replying_via(Some(b20)));
    assert_eq!(
        (reply.path(), reply.outcome),
        (vec![e50, c30, a10], Outcome::Delivered)
    );
}

/// Regression: when a node has a stale `bridged[T] = C` hint (`C-T` disconnected) alongside a
/// closer live greedy forward peer `B` (`A=10 < C=20 < B=30 < T=40` with links `A-C`, `A-B`,
/// `B-T`), `delivery_step` prefers the viable ordinary forward `A -> B -> T` instead of bouncing
/// into the stale bridge `C`.
#[test]
fn test_stale_bridge_hint_does_not_override_viable_greedy_forward() {
    let [a10, c20, b30, t40] = [10u32, 20, 30, 40].map(Did::from);

    let mut view_a = TopologyState::new(a10, vec![c20, b30], Some(t40), vec![None; 8]);
    view_a.bridged.insert(t40, c20);

    let view_c = TopologyState::new(c20, vec![b30, t40, a10], Some(a10), vec![None; 8]);
    let view_b = TopologyState::new(b30, vec![t40, a10, c20], Some(c20), vec![None; 8]);
    let view_t = TopologyState::new(t40, vec![a10, c20, b30], Some(b30), vec![None; 8]);

    let views = BTreeMap::from([(a10, view_a), (c20, view_c), (b30, view_b), (t40, view_t)]);
    let links = [(a10, c20), (a10, b30), (b30, t40)];
    let overlay = Overlay::with_links(views, links);

    let route = overlay.check_route_law(a10, t40, RouteStage::TOWARD);
    assert_eq!(
        (route.path(), route.outcome),
        (vec![a10, b30, t40], Outcome::Delivered)
    );
}
