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
//! Scope: this validates routing *correctness*, not bootstrap convergence —
//! discovery from a star is notify/connection-driven and is covered by the
//! stage-2 model and `dht_convergence` (which also owns the finger-table
//! fixpoint). Routing here is run on the converged (full-knowledge) ring, where
//! `closest_predecessor` always has a closer node, so production never
//! self-routes — asserted below rather than worked around.

use num_bigint::BigUint;

use super::dht_convergence::spec;
use super::dht_convergence::K;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::storage::MemStorage;

/// Safety bound on routing hops (a converged ring resolves in O(log n)).
const MAX_HOPS: usize = 64;

fn did_frac(num: u64, den: u64) -> Did {
    Did::from((BigUint::from(1u8) << 160) * BigUint::from(num) / BigUint::from(den))
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
    /// pair of adjacent nodes, including the wrap gap). This exercises the
    /// production routing the stage-2 model abstracts and shows it agrees with
    /// the spec successor. (Targets are strictly between nodes: for a target
    /// equal to a node's own DID, `find_successor` returns that node's successor
    /// — the Chord boundary convention — which is a separate semantics question,
    /// not routing.)
    #[test]
    fn real_find_successor_routing_is_correct() {
        for n in 3..=6u64 {
            let all: Vec<Did> = (0..n).map(|i| did_frac(i, n)).collect();
            let rings = converged_rings(&all);

            let targets: Vec<Did> = (0..n).map(|i| did_frac(2 * i + 1, 2 * n)).collect();

            for origin in 0..all.len() {
                for &target in &targets {
                    assert_eq!(
                        route(&rings, &all, origin, target),
                        true_successor(&all, target),
                        "wrong successor (n={n}, origin={origin}, target={target})"
                    );
                }
            }
        }
    }
}
