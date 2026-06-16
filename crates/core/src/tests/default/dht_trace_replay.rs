//! Tier 2 trace-validation — routing correctness of the **real** multi-hop
//! `PeerRing::find_successor`.
//!
//! The stage-2 Stateright model abstracts routing with a single-hop spec
//! successor (`successor_of`) and explores message interleavings. This test is
//! its counterpart: on a *converged* ring of real `PeerRing`s it runs the
//! production multi-hop routing — `find_successor` returning either `Some` or a
//! `RemoteAction(next, _)` that is forwarded to `closest_predecessor`, hop after
//! hop across the real nodes — and asserts it resolves to the true successor of
//! arbitrary ring positions. So the REAL routing is exercised here and shown
//! correct, which is the equivalence the abstraction relies on (followed to
//! completion over full knowledge, `find_successor` == the nearest forward node).
//!
//! Scope (narrow): this validates routing *correctness* only, on an
//! already-converged ring. The other layers each cover a different thing:
//!   * `dht_convergence` proves the deterministic safety FIXPOINT — the
//!     converged state is correct, not that it is reached;
//!   * `dht_schedule` drives the six clustered DIDs to that fixpoint
//!     deterministically under representative controlled delivery orders (FIFO /
//!     LIFO), replacing the old wall-clock-bounded flaky convergence test;
//!   * the stage-2 Stateright model is an N=3 routing ABSTRACTION (no
//!     `fix_fingers`, not the six clustered DIDs + K=3 regime).
//!
//! Routing here is run on the converged (full-knowledge) ring, where
//! `closest_predecessor` always has a closer node, so production never
//! self-routes — asserted below rather than worked around.

use num_bigint::BigUint;

use super::dht_convergence::spec;
use super::dht_convergence::Layout;
use super::dht_convergence::K;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::storage::MemStorage;

/// Safety bound on routing hops (a converged ring resolves in O(log n)).
const MAX_HOPS: usize = 64;

/// A point strictly between each pair of ring-adjacent nodes (sorted by ring
/// position, including the wrap gap) — an unambiguous lookup target whose
/// successor is the node just clockwise of it, for any layout.
fn midpoints(all: &[Did]) -> Vec<Did> {
    let m = BigUint::from(1u8) << 160;
    let mut pos: Vec<BigUint> = all.iter().map(|&d| BigUint::from(d)).collect();
    pos.sort();
    let n = pos.len();
    (0..n)
        .map(|i| {
            let a = pos[i].clone();
            let b = if i + 1 < n {
                pos[i + 1].clone()
            } else {
                &pos[0] + &m
            };
            Did::from((a + b) / BigUint::from(2u8))
        })
        .collect()
}

/// `n` real `PeerRing`s in their converged state: each has joined every other
/// node (full knowledge), so successor lists and finger tables are populated.
fn converged_rings(all: &[Did]) -> Vec<PeerRing> {
    all.iter()
        .map(|&me| {
            let dht = PeerRing::new_with_storage(me, K as u8, Box::new(MemStorage::new()));
            for &other in all {
                if other != me {
                    let _ = dht.join(other);
                }
            }
            dht
        })
        .collect()
}

/// Production multi-hop `find_successor`: start at `origin`, and while the node
/// returns `RemoteAction(next, _)`, forward to `next` (the real
/// `closest_predecessor`) and continue there — exactly `reset_destination`.
/// Returns the resolved successor.
fn route(rings: &[PeerRing], all: &[Did], origin: usize, target: Did) -> Did {
    let idx = |did: Did| all.iter().position(|&d| d == did).expect("did in set");
    let mut at = origin;
    for _ in 0..MAX_HOPS {
        match rings[at].find_successor(target).unwrap() {
            PeerRingAction::Some(did) => return did,
            PeerRingAction::RemoteAction(next, _) => {
                let ni = idx(next);
                // On a converged ring `closest_predecessor` always finds a node
                // strictly closer to the target, so a hop never lands back on the
                // same node. (A self-route is the sparse-bootstrap pathology this
                // converged-ring test deliberately excludes.)
                assert_ne!(ni, at, "production find_successor self-routed at {at}");
                at = ni;
            }
            other => panic!("unexpected find_successor action: {other:?}"),
        }
    }
    panic!("find_successor routing did not resolve within {MAX_HOPS} hops");
}

/// The true successor of a ring position: the nearest node going forward.
fn true_successor(all: &[Did], target: Did) -> Did {
    *all.iter()
        .min_by_key(|&&d| spec::dist(target, d))
        .expect("non-empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From every origin, the real multi-hop `find_successor` resolves to the
    /// true successor — for arbitrary ring positions (midpoints between every
    /// pair of adjacent nodes, including the wrap gap), across the SAME
    /// representative finger-table regimes `dht_convergence` uses: evenly spaced,
    /// `2^k`-aligned (fully populated), the clustered six production DIDs
    /// (collapsed fingers — the hard `closest_predecessor` branch), and the
    /// dyadic boundary. (Targets are strictly between nodes: for a target equal
    /// to a node's own DID, `find_successor` returns that node's successor — the
    /// Chord boundary convention — which is a separate semantics question, not
    /// routing.)
    #[test]
    fn real_find_successor_routing_is_correct() {
        let layouts = [
            Layout::Even(5),
            Layout::Pow2(8),
            Layout::Clustered,
            Layout::DyadicBoundary,
        ];
        for layout in layouts {
            let all = layout.dids();
            let rings = converged_rings(&all);
            let targets = midpoints(&all);

            for origin in 0..all.len() {
                for &target in &targets {
                    assert_eq!(
                        route(&rings, &all, origin, target),
                        true_successor(&all, target),
                        "wrong successor (origin={origin}, target={target}, all={all:?})"
                    );
                }
            }
        }
    }
}
