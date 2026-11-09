//! Laws of the HMCC/Zave stabilize merge.

use super::*;

/// `rank(p)`: one plus the number of candidates strictly nearer to `local`
/// than `p`, the position `p` would take in the normalized sequence.
fn clockwise_rank(local: Did, candidates: &[Did], peer: Did) -> usize {
    let nearer = candidates
        .iter()
        .copied()
        .filter(|candidate| dist(local, *candidate) < dist(local, peer))
        .count();
    nearer + 1
}

/// Law: `∀p ∈ reported. p ∈ stabilize_successors(...) ⇔ rank(p) ≤ K`,
/// including the last entry of a report shorter than capacity (#786: a
/// report carries no terminal self entry, so its last entry is a real
/// successor).
#[test]
fn test_stabilize_retains_every_reported_successor_within_capacity() {
    let local = did(0);
    let current = [did(8)];
    let predecessor = did(1);
    for reported in [vec![did(2)], vec![did(2), did(4)], vec![
        did(2),
        did(4),
        did(6),
    ]] {
        let stabilized = stabilize_successors(local, &current, &reported, Some(predecessor), 3);
        let known = current
            .iter()
            .copied()
            .chain([predecessor])
            .chain(reported.iter().copied())
            .collect::<Vec<_>>();
        for peer in reported.iter().copied() {
            let rank = clockwise_rank(local, &known, peer);
            assert_eq!(
                stabilized.contains(&peer),
                rank <= 3,
                "reported {peer} ranks {rank} in {stabilized:?}"
            );
        }
    }
}

/// Law (Def. 3 & Lemma 1): at an articulation node `C(50)` with successor head `D(80)`,
/// a closer predecessor `E(40)` supersedes an earlier `B(30)`, and when `B(30)` re-notifies
/// on its next stabilization tick, `C(50)` retains `[E(40), B(30)]` ordered by descending
/// `dist(C, p)` and references both in `referenced_peers()`.
#[test]
fn test_multi_predecessor_rectification_retains_sub_ring_predecessors_and_supersedes_on_closer() {
    let c = did(50);
    let d = did(80);
    let b = did(30);
    let e = did(40);
    let a = did(10);

    let initial = state(c, vec![d, a], None, vec![None; 6], 0);
    let after_b = step(
        &initial,
        TopologyEvent::NotifyWithinTopology { predecessor: b },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(after_b.predecessor, Some(b));
    assert_eq!(after_b.predecessors, vec![b]);

    // Closer joiner `E(40)` supersedes `B(30)`.
    let after_e = step(
        &after_b,
        TopologyEvent::NotifyWithinTopology { predecessor: e },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(after_e.predecessor, Some(e));
    assert_eq!(after_e.predecessors, vec![e]);

    // Sub-ring predecessor `B(30)` re-notifies on its next tick and is retained behind `E(40)`.
    let converged = step(
        &after_e,
        TopologyEvent::NotifyWithinTopology { predecessor: b },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(converged.predecessor, Some(e));
    assert_eq!(converged.predecessors, vec![e, b]);
    assert!(converged.references(e));
    assert!(converged.references(b));
    assert!(converged.is_well_formed(DEFAULT_SUCCESSOR_CAPACITY));
}

/// Law (Theorem 2): passive 1-hop bridge hints recorded from an admitted successor allow
/// `find_successor` to resolve targets in the unreachable gap `(local, head)` while preserving
/// `RoutesClockwise` on every target.
#[test]
fn test_record_bridged_peers_and_gap_aware_find_successor_preserve_routes_clockwise() {
    let d = did(80);
    let e = did(40);
    let c = did(50);
    let a = did(10);
    let b = did(30);

    let initial = state(d, vec![e, c], Some(c), vec![None; 8], 0);
    let with_bridges = step(
        &initial,
        TopologyEvent::RecordBridgedPeers {
            reporter: c,
            unconfirmed: vec![a, b],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;

    assert_eq!(with_bridges.bridged.get(&a), Some(&c));
    assert_eq!(with_bridges.bridged.get(&b), Some(&c));
    assert_eq!(
        find_successor(&with_bridges, a),
        FindSuccessorStep::Local(a)
    );
    assert_eq!(
        find_successor(&with_bridges, did(20)),
        FindSuccessorStep::Local(b)
    );
    assert_eq!(
        find_successor(&with_bridges, b),
        FindSuccessorStep::Local(b)
    );
    assert_eq!(
        find_successor(&with_bridges, did(35)),
        FindSuccessorStep::Local(e)
    );
    for target in 0..100u32 {
        assert!(with_bridges.routes_clockwise_toward(did(target)));
    }
}
