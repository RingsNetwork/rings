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

/// Law (`#1221` Item 2): passive 1-hop bridge hints recorded from an admitted successor are
/// retained in `TopologyState::bridged`, while `topology::find_successor` returns only
/// referenced topology peers (`Local(head)`) so local storage ownership (`HoldAuthority`) and
/// `StorageSync` never shrink or target unconnected bridged peers.
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
        FindSuccessorStep::Local(e)
    );
    assert_eq!(
        find_successor(&with_bridges, did(20)),
        FindSuccessorStep::Local(e)
    );
    assert_eq!(
        find_successor(&with_bridges, b),
        FindSuccessorStep::Local(e)
    );
    assert_eq!(
        find_successor(&with_bridges, did(35)),
        FindSuccessorStep::Local(e)
    );
    for target in 0..100u32 {
        assert!(with_bridges.routes_clockwise_toward(did(target)));
    }
}

/// Regression (`#1221` Item 1): in `step_record_bridged_peers`, a farther successor cannot
/// overwrite an existing `(peer, closer_successor)` bridge entry, while a closer successor
/// can replace a farther successor's entry.
#[test]
fn test_record_bridged_peers_prevents_farther_reporter_from_overwriting_closer_successor() {
    let local = did(10);
    let closer = did(20);
    let farther = did(40);
    let target = did(60);

    let initial = state(local, vec![closer, farther], None, vec![None; 8], 0);
    let after_closer = step(
        &initial,
        TopologyEvent::RecordBridgedPeers {
            reporter: closer,
            unconfirmed: vec![target],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(after_closer.bridged.get(&target), Some(&closer));

    // Farther successor reporting the same target must not overwrite `closer`.
    let after_farther = step(
        &after_closer,
        TopologyEvent::RecordBridgedPeers {
            reporter: farther,
            unconfirmed: vec![target],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(after_farther.bridged.get(&target), Some(&closer));

    // Conversely, if `farther` reported first, `closer` may supersede it.
    let seeded_farther = step(
        &initial,
        TopologyEvent::RecordBridgedPeers {
            reporter: farther,
            unconfirmed: vec![target],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(seeded_farther.bridged.get(&target), Some(&farther));
    let superseded_by_closer = step(
        &seeded_farther,
        TopologyEvent::RecordBridgedPeers {
            reporter: closer,
            unconfirmed: vec![target],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(superseded_by_closer.bridged.get(&target), Some(&closer));
}

/// Regression (`#1221` Item 3): when a 2-node wrapping ring `{local(10), head(30)}` with
/// `predecessor = Some(head(30))` admits a 3rd peer `50`, `head(30)` is immediately pruned
/// from `predecessor` and `predecessors`.
#[test]
fn test_two_node_predecessor_is_pruned_when_ring_expands() {
    let local = did(10);
    let head = did(30);
    let third = did(50);

    let two_node = state(local, vec![head], Some(head), vec![None; 8], 0);
    let expanded = step(
        &two_node,
        TopologyEvent::Admit {
            peer: third,
            deferred_proof: None,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(expanded.successors, vec![head, third]);
    assert_eq!(expanded.predecessor, None);
    assert!(expanded.predecessors.is_empty());
}

/// Regression (`#1221` Item 4): `SuccessorRemoval::PromoteReplacements` promotes routable
/// replacements inside `topology::step`, updating finger hints and emitting
/// `TopologyAction::SuccessorHeadChanged`.
#[test]
fn test_remove_with_promote_replacements_updates_fingers_and_emits_head_change() {
    let local = did(10);
    let leaving_head = did(20);
    let replacement = did(30);

    let initial = state(
        local,
        vec![leaving_head],
        None,
        vec![Some(leaving_head); 8],
        0,
    );
    let removed = step(
        &initial,
        TopologyEvent::Remove {
            peer: leaving_head,
            successor: crate::dht::topology::SuccessorRemoval::PromoteReplacements(vec![
                replacement,
            ]),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(removed.state.successors, vec![replacement]);
    assert!(removed.state.fingers.contains(&Some(replacement)));
    assert!(removed
        .actions
        .contains(&TopologyAction::SuccessorHeadChanged(replacement)));
}

/// Law (Capacity Bound): `TopoInfo::unconfirmed_peers` and `TopologyEvent::RecordBridgedPeers`
/// bound per-reporter entries to `2 * capacity` and total `bridged` entries across all
/// reporters to `2 * capacity^2`, even when an untrusted report advertises 1,000 peers.
#[test]
fn test_record_bridged_peers_enforces_per_reporter_and_total_capacity_bounds() {
    let local = did(10);
    let r1 = did(20);
    let r2 = did(30);
    let r3 = did(40);
    let capacity = DEFAULT_SUCCESSOR_CAPACITY;

    let oversized_report = crate::dht::TopoInfo {
        successors: (100u32..600).map(did).collect(),
        predecessor: Some(did(600)),
        predecessors: (601u32..1100).map(did).collect(),
    };
    let unconfirmed = oversized_report.unconfirmed_peers(local, capacity, |_| false);
    assert_eq!(unconfirmed.len(), 2 * capacity);

    let initial = state(local, vec![r1, r2, r3], None, vec![None; 8], 0);
    let after_r1 = step(
        &initial,
        TopologyEvent::RecordBridgedPeers {
            reporter: r1,
            unconfirmed: (100u32..1100).map(did).collect(),
        },
        capacity,
    )
    .state;
    assert_eq!(after_r1.bridged.len(), 2 * capacity);

    // Seed an oversized initial bridged map and verify `RecordBridgedPeers` truncates to `2K^2`.
    let mut artificially_oversized = after_r1.clone();
    for id in 2000u32..2050 {
        artificially_oversized.bridged.insert(did(id), r2);
    }
    let after_r2 = step(
        &artificially_oversized,
        TopologyEvent::RecordBridgedPeers {
            reporter: r3,
            unconfirmed: (3000u32..3020).map(did).collect(),
        },
        capacity,
    )
    .state;
    assert!(
        after_r2.bridged.len() <= 2 * capacity * capacity,
        "expected at most {}, got {}",
        2 * capacity * capacity,
        after_r2.bridged.len()
    );
}
