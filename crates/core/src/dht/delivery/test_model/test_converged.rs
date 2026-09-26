//! Delivery on the Chord fixpoint: stronger laws than the safety laws every overlay obeys.
//!
//! On `ChordFixpoint(M)` every view holds its true successors, predecessor, and the
//! sparse/no-wrap fingers `finger[i] = Finger(M, n, i)`, and every view peer is linked. Then:
//!
//! - `Delivered`: every route between members is delivered;
//! - `GreedyOnly`: no hop is a handoff; the route never crosses its destination;
//! - `Halving`: every hop that does not deliver satisfies `2·dist(next, T) < dist(n, T)`.
//!   With `2^i ≤ dist(n, T) < 2^(i+1)`, `finger[i]` is the nearest member at distance `≥ 2^i`,
//!   and `T` is such a member, so `finger[i] ∈ (n, T]`; greedy delivery picks a peer at least as
//!   far, which leaves less than `2^i`, i.e. less than half. A route therefore takes at most
//!   `⌈log₂ dist(n₀, T)⌉ + 1` hops;
//! - `NoDisclosure`: every node has a predecessor, so no request names a `reply_via`.

use rand::SeedableRng;
use rand_hc::Hc128Rng;

use super::converged;
use super::converged_view;
use super::random_members;
use super::Outcome;
use super::RouteStage;
use crate::dht::delivery::origination;
use crate::dht::topology::dist;

/// Ring sizes of the converged checks.
const SIZES: [usize; 6] = [2, 3, 5, 8, 13, 21];

/// `Delivered ∧ GreedyOnly ∧ Halving` for every ordered pair of members of fixed-seed random
/// rings of every size in [`SIZES`].
#[test]
fn test_converged_routes_deliver_greedily_halving_the_distance() {
    let mut rng = Hc128Rng::seed_from_u64(1);
    for size in SIZES {
        let members = random_members(&mut rng, size);
        for route in converged(&members).check_all_routes() {
            assert_eq!(route.outcome, Outcome::Delivered, "{route:?}");
            assert!(
                route
                    .states
                    .iter()
                    .all(|(_, stage)| *stage == RouteStage::TOWARD),
                "handoff on the fixpoint: {route:?}"
            );
            let path = route.path();
            let Some(destination) = path.last().copied() else {
                continue;
            };
            for (from, to) in path.iter().copied().zip(path.iter().copied().skip(1)) {
                if to != destination {
                    assert!(
                        dist(to, destination) * 2u8 < dist(from, destination),
                        "hop {from} → {to} does not halve the distance: {route:?}"
                    );
                }
            }
        }
    }
}

/// `NoDisclosure`: on the fixpoint every node's origination names no `reply_via`, whatever the
/// destination, so a converged node never discloses a link.
#[test]
fn test_converged_nodes_name_no_reply_via() {
    let mut rng = Hc128Rng::seed_from_u64(2);
    for size in SIZES {
        let members = random_members(&mut rng, size);
        for local in members.iter().copied() {
            let view = converged_view(&members, local);
            for destination in members.iter().copied().filter(|peer| *peer != local) {
                let decided = origination(&view, destination, |peer| members.contains(&peer));
                assert_eq!(decided.reply_via, None, "{local} → {destination}");
                assert!(decided.hop.is_some(), "{local} → {destination}");
            }
        }
    }
}
