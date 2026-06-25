//! Deterministic, transport-free convergence tests for the Chord DHT.
//!
//! These tests pin down the **safety** half of stabilization: for any fixed set
//! of DIDs the converged DHT (successor list, predecessor, finger table) is the
//! *unique fixpoint* of Chord's invariants, and the production code path
//! (`PeerRing::{join,notify}` + `finger.join`) must materialise exactly that
//! fixpoint. There is no transport and no message ordering here, so the result
//! is fully deterministic — the *liveness* half (does the async protocol reach
//! the fixpoint under arbitrary message interleavings?) is intentionally NOT
//! tested here; it is covered by the integration test (`test_stabilization_*`,
//! which polls) and, in the future, by model-checking the TLA+ `Spec` below.
//!
//! ====================================================================
//! FORMAL SPEC (TLA+) — the constraint these tests discharge.
//! Written so it can be lifted verbatim into a `.tla` module and checked
//! with TLC (finite instance) / proved with TLAPS (parametric in N).
//!
//! ---- MODULE ChordConvergence ----------------------------------------
//! EXTENDS Integers, FiniteSets, Sequences
//!
//! CONSTANTS
//!   M,     \* ring order, M = 2^160      (Did is Z/M, see dht/did.rs)
//!   K,     \* successor capacity         (dht_succ_max = 3)
//!   Node,  \* finite set of nodes
//!   Id     \* Id \in [Node -> 0..M-1], the ring position of each node
//!
//! ASSUME Ring ==
//!   /\ M = 2^160  /\ K = 3
//!   /\ Id \in [Node -> 0..(M-1)]
//!   /\ \A a, b \in Node : a # b => Id[a] # Id[b]   \* DIDs distinct
//!
//! \* Clockwise distance from a to b. This IS BiasId: re-centering the ring at
//! \* a, dist(a,b) = (Id[b]-Id[a]) % M = BiasId::new(Id[a],Id[b]).pos().
//! dist(a, b) == (Id[b] - Id[a]) % M
//! Others(n)  == Node \ {n}
//! Min(a, b)  == IF a <= b THEN a ELSE b
//!
//! \* Rank of x among Others(n) by increasing clockwise distance (0-based).
//! Rank(n, x) == Cardinality({ y \in Others(n) : dist(n,y) < dist(n,x) })
//! SeqSet(s) == { s[i] : i \in 1..Len(s) }
//! RankIn(S, n, x) == Cardinality({ y \in S \ {n} : dist(n,y) < dist(n,x) })
//!
//! \* Successors(n): the K nearest forward nodes, ordered by distance. Mirrors
//! \* SuccessorSeq::update (keep the K smallest dist, sorted).
//! SuccessorsOver(S, n) ==
//!   [ i \in 1..Min(K, Cardinality(S \ {n})) |->
//!       CHOOSE x \in S \ {n} : RankIn(S, n, x) = i - 1 ]
//! Successors(n) == SuccessorsOver(Node, n)
//!
//! \* Predecessor(n): nearest node behind = farthest forward. Mirrors
//! \* chord.rs `notify`: pred := d iff bias(pred) < bias(d).
//! Predecessor(n) ==
//!   CHOOSE x \in Others(n) : \A y \in Others(n) : dist(n,x) >= dist(n,y)
//!
//! \* Finger(n, k): nearest forward node at clockwise distance >= 2^k, else
//! \* "none". Fingers are 2^k spaced, NOT linear.
//! \*
//! \* DEVIATION from the Chord paper (intentional): the paper's finger[k] is
//! \* successor((n + 2^k) mod M), which WRAPS, so it is always a live node. This
//! \* operator returns "none" when no known node is at distance >= 2^k (no wrap)
//! \* — deliberately, because it mirrors Rings' `finger.join`, which leaves
//! \* finger[k] = None in that case. So these tests verify Rings' sparse/no-wrap
//! \* finger table, not the paper-accurate wrapping one. Routing/liveness here do
//! \* not lean on high-index wrap fingers (successor list + low/mid fingers
//! \* carry it); a paper-accurate wrapping spec would be a separate exercise.
//! Finger(n, k) ==
//!   LET C == { x \in Others(n) : dist(n,x) >= 2^k } IN
//!   IF C = {} THEN none
//!   ELSE CHOOSE x \in C : \A y \in C : dist(n,x) <= dist(n,y)
//!
//! \* CorrectChord stabilize operation (HMCC/Zave path). This is the default
//! \* production path: `Stabilizer::stabilize` calls `correct_stabilize`, and
//! \* message handling applies `PeerRing::stabilize` to the successor's TopoInfo.
//! ButLast(s) == IF Len(s) = 0 THEN <<>> ELSE SubSeq(s, 1, Len(s)-1)
//! InsertKnown(n, cur, candidates) ==
//!   LET Known == {n} \cup SeqSet(cur) \cup candidates IN
//!     SuccessorsOver(Known, n)
//! Improved(n, cur, p) ==
//!   /\ p # none
//!   /\ p # n
//!   /\ (Len(cur) = 0 \/ dist(n, p) < dist(n, Head(cur)))
//! CorrectStabilize(n, cur, topoSucc, topoPred) ==
//!   LET predSet == IF topoPred = none THEN {} ELSE {topoPred}
//!       cand == SeqSet(ButLast(topoSucc)) \cup predSet
//!       next == InsertKnown(n, cur, cand) IN
//!   /\ succ'[n] = next
//!   /\ query'  = IF Improved(n, cur, topoPred) THEN <<topoPred>> ELSE <<>>
//!   /\ notify' = IF Len(next) = 0 THEN <<>> ELSE <<Head(next)>>
//!
//! \* CorrectChord rectify operation (HMCC/Zave path). A notify message from p
//! \* carries one candidate predecessor. Rectify adopts it only when it is
//! \* closer behind n than the current predecessor.
//! RectifyPred(n, curPred, p) ==
//!   IF curPred = none \/ dist(n, curPred) < dist(n, p) THEN p ELSE curPred
//! CorrectRectify(n, curPred, p) ==
//!   /\ p # n
//!   /\ pred'[n] = RectifyPred(n, curPred, p)
//!
//! \* Converged(n): the materialised local state equals the operators above.
//! Converged(n) ==
//!   /\ succ[n]  = Successors(n)
//!   /\ pred[n]  = Predecessor(n)
//!   /\ finger[n] = [ k \in 0..159 |-> Finger(n, k) ]
//!
//! THEOREM Safety == \A n \in Node : Converged(n)  is the UNIQUE fixpoint, and
//!   the production join/notify path materialises it. This is what every test
//!   below asserts — for a covering set of DID layouts (see `Layout`).
//!
//! \* INDUCTION over the node count N (the structure these tests are organised
//! \* around): let P(N) == "for every Node with |Node| = N satisfying Ring,
//! \* the converged state = the operators above".
//! \*   Base   : P(2)  (one forward neighbour, the wrap case).
//! \*   Step   : P(N) => P(N+1). join/notify/finger.join are monotone in the
//! \*            known-node set and the operators are defined by the same
//! \*            min/max-by-dist, so inserting a node only refines each slot
//! \*            toward a strictly-closer candidate, preserving the equality.
//! \* The base cases (N=2,3) and several steps (N up to 8) are discharged
//! \* concretely below; the general P(N) is the TLAPS obligation.
//! ====================================================================

use std::str::FromStr;

use num_bigint::BigUint;

use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Chord;
use crate::dht::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::TopoInfo;
use crate::error::Result;
use crate::storage::MemStorage;

/// Successor-list capacity; matches `SwarmBuilder::dht_succ_max` (production).
pub(super) const K: usize = 3;
/// Ring bit-width; `Did` is `Z/2^160`, the finger table has one slot per bit.
const BITS: usize = 160;

/// The formal operators — a direct, independent Rust mirror of the TLA+ spec in
/// the module doc. Kept deliberately tiny and obviously-correct so they serve as
/// the *specification* that the production DHT code is checked against. Shared
/// (the `mod spec` pivot) with the Stateright protocol model in `dht_stateright`.
pub(super) mod spec {
    use super::*;

    /// `dist(a,b) == (Id[b] - Id[a]) % 2^160` — clockwise distance / `BiasId.pos()`.
    pub fn dist(a: Did, b: Did) -> BigUint {
        // `Did - Did` is already modular subtraction on Z/2^160 (see dht/did.rs).
        BigUint::from(b - a)
    }

    /// `Successors(n)` — the K nearest forward nodes, in increasing distance.
    pub fn successors(all: &[Did], n: Did) -> Vec<Did> {
        let mut others: Vec<Did> = all.iter().copied().filter(|&d| d != n).collect();
        others.sort_by_key(|&d| dist(n, d));
        others.truncate(K);
        others
    }

    /// `Predecessor(n)` — the farthest-forward node (= nearest behind self).
    pub fn predecessor(all: &[Did], n: Did) -> Option<Did> {
        all.iter()
            .copied()
            .filter(|&d| d != n)
            .max_by_key(|&d| dist(n, d))
    }

    /// `CorrectRectify` predecessor transition.
    pub fn correct_rectify_predecessor(me: Did, current: Option<Did>, pred: Did) -> Option<Did> {
        match current {
            Some(cur) if dist(me, cur) >= dist(me, pred) => Some(cur),
            _ => Some(pred),
        }
    }

    /// `Finger(n, bit)` — nearest forward node at distance `>= 2^bit`, else None.
    /// Mirrors Rings' `finger.join` (no wrap: None when nothing is far enough),
    /// which intentionally differs from the Chord paper's wrapping
    /// `successor((n + 2^bit) mod M)`. See the module-doc DEVIATION note.
    pub fn finger(all: &[Did], n: Did, bit: usize) -> Option<Did> {
        let threshold = BigUint::from(1u8) << bit;
        all.iter()
            .copied()
            .filter(|&d| d != n && dist(n, d) >= threshold)
            .min_by_key(|&d| dist(n, d))
    }

    /// The full per-node finger table the spec predicts (one slot per ring bit).
    pub fn finger_table(all: &[Did], n: Did) -> Vec<Option<Did>> {
        (0..BITS).map(|bit| finger(all, n, bit)).collect()
    }

    fn push_unique(xs: &mut Vec<Did>, x: Did) {
        if !xs.contains(&x) {
            xs.push(x);
        }
    }

    /// `CorrectStabilize` successor update: merge current successors with the
    /// successor's predecessor and all but the last entry of the successor's
    /// successor list, then keep the K nearest forward nodes.
    pub fn correct_stabilize_successors(
        me: Did,
        current: &[Did],
        topo_successors: &[Did],
        topo_predecessor: Option<Did>,
    ) -> Vec<Did> {
        let mut known = vec![me];
        for &did in current {
            push_unique(&mut known, did);
        }
        if let Some(pred) = topo_predecessor {
            push_unique(&mut known, pred);
        }
        for &did in topo_successors
            .iter()
            .take(topo_successors.len().saturating_sub(1))
        {
            push_unique(&mut known, did);
        }
        successors(&known, me)
    }

    /// `CorrectStabilize` query side effect: ask the successor's predecessor for
    /// its successor list only when that predecessor is a strict improvement over
    /// the old head of the local successor list.
    pub fn correct_stabilize_query(
        me: Did,
        current: &[Did],
        topo_predecessor: Option<Did>,
    ) -> Option<Did> {
        let pred = topo_predecessor?;
        if pred == me {
            return None;
        }
        let old_head = current.iter().copied().min_by_key(|&did| dist(me, did));
        match old_head {
            Some(head) if dist(me, pred) >= dist(me, head) => None,
            _ => Some(pred),
        }
    }

    /// `CorrectStabilize` notify side effect: notify the new successor, if any.
    pub fn correct_stabilize_notify(me: Did, next_successors: &[Did]) -> Option<Did> {
        next_successors.first().copied().filter(|&did| did != me)
    }
}

/// The ring order, `2^160`.
fn ring() -> BigUint {
    BigUint::from(1u8) << BITS
}

/// A DID at `num/den` of the way round the ring (used for even spacing).
fn did_frac(num: u64, den: u64) -> Did {
    Did::from(ring() * BigUint::from(num) / BigUint::from(den))
}

/// A DID at ring position `2^bit`.
fn did_pow(bit: usize) -> Did {
    Did::from(BigUint::from(1u8) << bit)
}

/// A placement strategy for the DID set under test. This is the test abstraction:
/// every layout yields a `Vec<Did>`, and the assertion is identical across all of
/// them — only the ring structure changes, exercising different operator branches.
/// Shared with `dht_trace_replay` so the real-routing test covers the same
/// representative finger-table regimes.
pub(super) enum Layout {
    /// `n` evenly-spaced nodes. Smallest interesting rings / the base cases.
    Even(usize),
    /// `n` nodes at `2^k`-aligned offsets, so each finger slot resolves to a
    /// distinct node — the *fully populated* finger table Chord is designed for.
    Pow2(usize),
    /// The six pathologically-clustered production addresses from the integration
    /// test (gaps spanning 0.61%..37%): immediate successors are only told apart
    /// by high-index fingers. The collapsed-finger regime.
    Clustered,
    /// A node placed *exactly* on a `2^k` boundary, so `dist == 2^k`: exercises
    /// the `>= 2^k` boundary (dyadic tie) in `finger.join` / `Finger`.
    DyadicBoundary,
}

impl Layout {
    /// The DID set for this layout (deterministic, distinct, no keys/transport).
    pub(super) fn dids(&self) -> Vec<Did> {
        match *self {
            Layout::Even(n) => (0..n as u64).map(|i| did_frac(i, n as u64)).collect(),
            // Nodes at 2^(BITS-1), 2^(BITS-2), ..., 2^(BITS-n): doubling gaps, so
            // from the smallest node each finger probe `+2^k` lands on a new node.
            Layout::Pow2(n) => (1..=n).map(|i| did_pow(BITS - i)).collect(),
            Layout::Clustered => [
                "0xcc13321381c4be4d3264588d4573c9529c0167a0",
                "0xdbf2d77c3a8bb59379009ec2ec423b8b58d60dbe",
                "0xd9863aad3267eaadca60adf51464e16d6f79465b",
                "0x8a5f987d1c2cc0fd6e0083df22ba9bd802706348",
                "0x2b5d1f769f346a08cee37f7382495b01126d480a",
                "0xca82ac762999ef4438d09223b01f9bf194cea94e",
            ]
            .iter()
            .map(|s| Did::from_str(s).unwrap())
            .collect(),
            // node0 at 0; node1 exactly 2^100 ahead (the tie); node2 in the far
            // half so the wrap/predecessor branch is also exercised.
            Layout::DyadicBoundary => {
                vec![Did::from(0u32), did_pow(100), did_pow(159)]
            }
        }
    }
}

/// Build the converged DHT for `node` by feeding it every other DID through the
/// production code path (`join` updates successor + finger, `notify` updates
/// predecessor). With full knowledge this is exactly the fixpoint the async
/// stabilizer is supposed to reach.
fn converged_dht(node: Did, all: &[Did]) -> PeerRing {
    let dht = PeerRing::new_with_storage(node, K as u8, Box::new(MemStorage::new()));
    for &other in all {
        if other != node {
            dht.join(other).unwrap();
            dht.notify(other).unwrap();
        }
    }
    dht
}

/// The single parametric assertion (the inductive invariant, instantiated): the
/// production converged state equals the formal operators, for every node.
fn assert_converged_matches_spec(layout: &Layout) {
    let dids = layout.dids();
    assert!(dids.len() >= 2, "need at least two nodes");

    for &n in &dids {
        let dht = converged_dht(n, &dids);

        assert_eq!(
            dht.successors().list().unwrap(),
            spec::successors(&dids, n),
            "successor list mismatch at node {n}"
        );
        assert_eq!(
            *dht.lock_predecessor().unwrap(),
            spec::predecessor(&dids, n),
            "predecessor mismatch at node {n}"
        );
        assert_eq!(
            dht.lock_finger().unwrap().list().clone(),
            spec::finger_table(&dids, n),
            "finger table mismatch at node {n}"
        );
    }
}

fn dht_with_successors(me: Did, successors: &[Did]) -> PeerRing {
    let dht = PeerRing::new_with_storage(me, K as u8, Box::new(MemStorage::new()));
    for &successor in successors {
        dht.successors().update(successor).unwrap();
    }
    dht
}

fn assert_correct_stabilize_matches_spec(
    me: Did,
    current_successors: &[Did],
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
) {
    let dht = dht_with_successors(me, current_successors);
    let action = dht
        .stabilize(TopoInfo {
            successors: topo_successors.to_vec(),
            predecessor: topo_predecessor,
        })
        .unwrap();
    let expected_successors = spec::correct_stabilize_successors(
        me,
        current_successors,
        topo_successors,
        topo_predecessor,
    );
    let mut expected_actions = vec![];
    if let Some(query) = spec::correct_stabilize_query(me, current_successors, topo_predecessor) {
        expected_actions.push(PeerRingAction::RemoteAction(
            query,
            PeerRingRemoteAction::QueryForSuccessorList,
        ));
    }
    if let Some(notify) = spec::correct_stabilize_notify(me, &expected_successors) {
        expected_actions.push(PeerRingAction::RemoteAction(
            notify,
            PeerRingRemoteAction::Notify(me),
        ));
    }

    assert_eq!(
        dht.successors().list().unwrap(),
        expected_successors,
        "CorrectStabilize successor list mismatch"
    );
    assert_eq!(
        action,
        PeerRingAction::MultiActions(expected_actions),
        "CorrectStabilize action mismatch"
    );
}

fn assert_correct_rectify_matches_spec(layout: &Layout) -> Result<()> {
    let dids = layout.dids();
    for &me in &dids {
        let current_predecessors = std::iter::once(None)
            .chain(dids.iter().copied().filter(|&did| did != me).map(Some))
            .collect::<Vec<_>>();

        for current in current_predecessors {
            for pred in dids.iter().copied().filter(|&did| did != me) {
                let dht = PeerRing::new_with_storage(me, K as u8, Box::new(MemStorage::new()));
                for other in dids.iter().copied().filter(|&did| did != me) {
                    let _ = dht.join(other)?;
                }
                {
                    let mut predecessor = dht.lock_predecessor()?;
                    *predecessor = current;
                }

                let expected = spec::correct_rectify_predecessor(me, current, pred);
                let successors_before = dht.successors().list()?;
                let fingers_before = dht.lock_finger()?.list().clone();

                dht.rectify(pred)?;

                assert_eq!(
                    *dht.lock_predecessor()?,
                    expected,
                    "CorrectRectify predecessor mismatch (me={me}, current={current:?}, pred={pred})"
                );
                assert_eq!(
                    dht.successors().list()?,
                    successors_before,
                    "CorrectRectify changed successors (me={me}, pred={pred})"
                );
                assert_eq!(
                    dht.lock_finger()?.list().clone(),
                    fingers_before,
                    "CorrectRectify changed fingers (me={me}, pred={pred})"
                );
            }
        }
    }
    Ok(())
}

/// Base case P(2): a single forward neighbour — the wrap-around case, where each
/// node's only successor and its predecessor are the same peer.
#[test]
fn convergence_base_n2() {
    assert_converged_matches_spec(&Layout::Even(2));
}

/// Operation conformance for the HMCC/Zave `CorrectRectify` operator:
/// predecessor notifications update only the predecessor slot, using the same
/// closest-behind rule as the formal model.
#[test]
fn correct_rectify_matches_predecessor_spec() -> Result<()> {
    for layout in [
        Layout::Even(3),
        Layout::Even(6),
        Layout::Pow2(8),
        Layout::Clustered,
        Layout::DyadicBoundary,
    ] {
        assert_correct_rectify_matches_spec(&layout)?;
    }
    Ok(())
}

/// Base case P(3): the smallest ring with a non-trivial successor/predecessor
/// distinction.
#[test]
fn convergence_base_n3() {
    assert_converged_matches_spec(&Layout::Even(3));
}

/// Inductive ladder P(2)..P(8) on evenly-spaced rings: discharges the base and
/// several inductive steps concretely (the general P(N) is the TLAPS obligation
/// in the module doc).
#[test]
fn convergence_inductive_ladder_even() {
    for n in 2..=8 {
        assert_converged_matches_spec(&Layout::Even(n));
    }
}

/// `2^k`-aligned ring (N=8): the finger table is *fully populated* — each slot
/// resolves to a distinct node — so this exercises the whole finger structure,
/// the regime Chord's `O(log N)` routing depends on.
#[test]
fn convergence_pow2_full_finger_n8() {
    assert_converged_matches_spec(&Layout::Pow2(8));
}

/// Pathologically-clustered ring (the six production addresses): the
/// collapsed-finger regime, where immediate successors are only distinguished by
/// high-index fingers. Same fixpoint correctness must hold.
#[test]
fn convergence_clustered_n6() {
    assert_converged_matches_spec(&Layout::Clustered);
}

/// Dyadic boundary: a node exactly `2^k` away, exercising the `>= 2^k` tie in the
/// finger construction.
#[test]
fn convergence_dyadic_boundary() {
    assert_converged_matches_spec(&Layout::DyadicBoundary);
}

/// Operation conformance for the HMCC/Zave `CorrectStabilize` operator:
/// a successor's predecessor that is closer than the old head is adopted,
/// queried for its successor list, then notified as the new successor.
#[test]
fn correct_stabilize_improved_predecessor_matches_spec() {
    let dids = Layout::Even(5).dids();
    assert_correct_stabilize_matches_spec(
        dids[0],
        &[dids[2]],
        &[dids[3], dids[4], dids[0]],
        Some(dids[1]),
    );
}

/// Even when the successor has no predecessor yet, `CorrectStabilize` still
/// notifies the current successor; the predecessor absence only suppresses the
/// improved-successor query.
#[test]
fn correct_stabilize_without_predecessor_still_notifies_successor() {
    let dids = Layout::Even(4).dids();
    assert_correct_stabilize_matches_spec(dids[0], &[dids[1]], &[dids[2], dids[3]], None);
}

/// A successor reporting this node as its predecessor is not an improved
/// successor and must not trigger a self-query.
#[test]
fn correct_stabilize_self_predecessor_does_not_query_self() {
    let dids = Layout::Even(3).dids();
    assert_correct_stabilize_matches_spec(dids[0], &[dids[1]], &[dids[2]], Some(dids[0]));
}

/// The production successor list is distance-sorted by `SuccessorSeq::update`;
/// the spec mirror must not depend on the raw order of test fixtures.
#[test]
fn correct_stabilize_unsorted_current_successors_matches_spec() {
    let dids = Layout::Even(6).dids();
    assert_correct_stabilize_matches_spec(
        dids[0],
        &[dids[3], dids[1]],
        &[dids[4], dids[5], dids[0]],
        Some(dids[2]),
    );
}

/// A predecessor that is farther than the old successor head may still be
/// learned as a backup successor, but must not trigger the improved-successor
/// query side effect.
#[test]
fn correct_stabilize_farther_predecessor_does_not_query() {
    let dids = Layout::Even(6).dids();
    assert_correct_stabilize_matches_spec(
        dids[0],
        &[dids[3], dids[1]],
        &[dids[4], dids[5]],
        Some(dids[2]),
    );
}

/// Duplicate candidates and self references are ignored by `SuccessorSeq`,
/// then the merged known set is truncated to the K nearest forward nodes.
#[test]
fn correct_stabilize_deduplicates_self_and_truncates_candidates() {
    let dids = Layout::Even(8).dids();
    assert_correct_stabilize_matches_spec(
        dids[0],
        &[dids[4], dids[0], dids[2], dids[2]],
        &[dids[1], dids[3], dids[5], dids[0]],
        Some(dids[2]),
    );
}

/// `CorrectStabilize` imports all but the last entry from the successor's
/// successor list. A close node in the last position must not be learned from
/// this operation.
#[test]
fn correct_stabilize_ignores_last_topo_successor() {
    let dids = Layout::Even(6).dids();
    assert_correct_stabilize_matches_spec(dids[0], &[dids[4]], &[dids[5], dids[1]], None);
}

/// Empty TopoInfo is a no-op when the node has no successor to notify.
#[test]
fn correct_stabilize_empty_topo_without_successor_is_noop() {
    let dids = Layout::Even(2).dids();
    assert_correct_stabilize_matches_spec(dids[0], &[], &[], None);
}
