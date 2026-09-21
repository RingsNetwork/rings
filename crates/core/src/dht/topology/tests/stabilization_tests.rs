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
