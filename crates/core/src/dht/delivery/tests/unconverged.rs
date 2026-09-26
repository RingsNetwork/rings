//! Delivery on stale views: each kind of staleness has its own expected outcome.
//!
//! | Staleness | Expected outcome |
//! |---|---|
//! | a successor list skips `T`, but some view links toward `T` | routed around, delivered |
//! | no view knows `T` and no greedy hop is linked to it | fails fast: one handoff, then the typed error |
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

/// Unknown destination, fails fast: from `A` no view on the route knows `T`, so the route hands
/// off once, to `C`, and `C` ends it with the typed error, where the owner-lookup route used to
/// circle `A → C → D → A` until the hop budget.
#[test]
fn test_unknown_destination_fails_fast_after_one_handoff() {
    let (overlay, [a, _, t, c, _]) = issue_865();

    let from_a = overlay.check_route_law(a, t, RouteStage::TOWARD);
    assert_eq!(from_a.path(), vec![a, c]);
    assert_eq!(from_a.outcome, Outcome::Unreachable);
    assert_eq!(
        from_a.states.last().map(|(_, stage)| *stage),
        Some(RouteStage::TOWARD.handed_off())
    );
}

/// A joiner, answered through `reply_via` (#873 §1.2): the joiner has no predecessor, so its
/// requests name its bootstrap, and every member's answer to it arrives; without the hint a
/// member not linked to the joiner fails fast instead.
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
