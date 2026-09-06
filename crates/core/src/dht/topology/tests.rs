use std::collections::BTreeSet;

use num_bigint::BigUint;

use super::*;

fn did(value: u32) -> Did {
    Did::from(value)
}

fn state(
    local: Did,
    successors: Vec<Did>,
    predecessor: Option<Did>,
    fingers: Vec<Option<Did>>,
    fix_finger_index: usize,
) -> TopologyState {
    TopologyState::new(local, successors, predecessor, fingers, fix_finger_index)
}

fn successor_distances(local: Did, successors: &[Did], capacity: usize) -> Vec<BigUint> {
    let infinity = BigUint::from(1u8) << RING_BITS;
    (0..capacity)
        .map(|index| {
            successors
                .get(index)
                .map(|successor| dist(local, *successor))
                .unwrap_or_else(|| infinity.clone())
        })
        .collect()
}

fn refines_successor_distances(before: &TopologyState, after: &TopologyState) -> bool {
    let before_distances =
        successor_distances(before.local, &before.successors, DEFAULT_SUCCESSOR_CAPACITY);
    let after_distances =
        successor_distances(after.local, &after.successors, DEFAULT_SUCCESSOR_CAPACITY);
    before_distances
        .iter()
        .zip(after_distances.iter())
        .all(|(before, after)| after <= before)
}

#[test]
fn test_join_step_updates_successors_fingers_and_connect_action() {
    let local = did(0);
    let peer = did(8);
    let next = step(
        &state(local, vec![], None, vec![None; 5], 0),
        TopologyEvent::Join { peer },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.successors, vec![peer]);
    assert_eq!(next.state.fingers, vec![
        Some(peer),
        Some(peer),
        Some(peer),
        Some(peer),
        None
    ]);
    assert_eq!(next.actions, vec![
        TopologyAction::FindSuccessorForConnect {
            next: peer,
            did: local
        },
        TopologyAction::SuccessorHeadChanged(peer),
    ]);
}

#[test]
fn test_join_step_refines_successor_distance_vector() {
    let local = did(0);
    let current = state(local, vec![did(20), did(40)], None, vec![None; 5], 0);
    let next = step(
        &current,
        TopologyEvent::Join { peer: did(10) },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert!(refines_successor_distances(&current, &next.state));
}

#[test]
fn test_stabilize_step_refines_successor_distance_vector() {
    let local = did(0);
    let current = state(local, vec![did(40)], None, vec![None; 5], 0);
    let next = step(
        &current,
        TopologyEvent::Stabilize {
            successors: vec![did(50), did(60)],
            predecessor: Some(did(10)),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert!(refines_successor_distances(&current, &next.state));
}

#[test]
fn test_remove_step_removes_peer_from_every_topology_slot() {
    let local = did(0);
    let peer = did(8);
    let next = step(
        &state(
            local,
            vec![peer],
            Some(peer),
            vec![Some(peer), Some(peer)],
            0,
        ),
        TopologyEvent::Remove {
            peer,
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert!(next.state.successors.is_empty());
    assert_eq!(next.state.predecessor, None);
    assert_eq!(next.state.fingers, vec![None, None]);
    assert!(next.actions.is_empty());
}

#[test]
fn test_ordinary_remove_does_not_promote_an_unverified_finger() {
    let local = did(0);
    let removed = did(8);
    let fallback = did(16);
    let next = step(
        &state(
            local,
            vec![removed],
            None,
            vec![Some(removed), None, Some(fallback)],
            0,
        ),
        TopologyEvent::Remove {
            peer: removed,
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert!(next.state.successors.is_empty());
    assert_eq!(next.state.fingers, vec![None, None, Some(fallback)]);
    assert!(next.actions.is_empty());
}

#[test]
fn test_remove_step_preserves_valid_slots_between_noncontiguous_peer_runs() {
    let local = did(0);
    let removed = did(8);
    let middle = did(16);
    let tail = did(32);
    let next = step(
        &state(
            local,
            vec![removed],
            None,
            vec![Some(removed), Some(middle), Some(removed), Some(tail)],
            0,
        ),
        TopologyEvent::Remove {
            peer: removed,
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fingers, vec![
        Some(middle),
        Some(middle),
        Some(tail),
        Some(tail)
    ]);
}

#[test]
fn test_unavailable_head_without_live_fallback_clears_unverified_successor_tail() {
    let local = did(0);
    let removed = did(8);
    let unverified = did(12);
    let next = step(
        &state(
            local,
            vec![removed, unverified],
            None,
            vec![Some(unverified)],
            0,
        ),
        TopologyEvent::Remove {
            peer: removed,
            successor: SuccessorRemoval::ReplaceWith(Vec::new()),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert!(next.state.successors.is_empty());
    assert_eq!(next.state.fingers, vec![Some(unverified)]);
    assert!(next.actions.is_empty());
}

#[test]
fn test_remove_step_replaces_unavailable_head_with_validated_successors_only() {
    let local = did(0);
    let removed = did(8);
    let unverified = did(12);
    let fallback = did(16);
    let verified_tail = did(24);
    let next = step(
        &state(
            local,
            vec![removed, unverified, fallback, verified_tail],
            None,
            vec![Some(unverified), Some(fallback), Some(verified_tail)],
            0,
        ),
        TopologyEvent::Remove {
            peer: removed,
            successor: SuccessorRemoval::ReplaceWith(vec![
                removed,
                verified_tail,
                fallback,
                fallback,
                local,
            ]),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.successors, vec![fallback, verified_tail]);
    assert_eq!(next.state.fingers, vec![
        Some(unverified),
        Some(fallback),
        Some(verified_tail)
    ]);
    assert_eq!(next.actions, vec![TopologyAction::SuccessorHeadChanged(
        fallback
    )]);
}

#[test]
fn test_admit_step_commits_join_and_pending_fingers_in_one_state() {
    let local = did(0);
    let peer = did(16);
    let next = step(
        &state(local, Vec::new(), None, vec![None; 5], 0),
        TopologyEvent::Admit {
            peer,
            fixed_fingers: vec![ConditionalFingerUpdate {
                index: 4,
                expected: None,
            }],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.successors, vec![peer]);
    assert_eq!(next.state.fingers, vec![
        Some(peer),
        Some(peer),
        Some(peer),
        Some(peer),
        Some(peer)
    ]);
    assert_eq!(next.actions, vec![
        TopologyAction::QuerySuccessorList(peer),
        TopologyAction::FindSuccessorForConnect {
            next: peer,
            did: local
        },
        TopologyAction::SuccessorHeadChanged(peer),
    ]);
}

#[test]
fn test_admit_step_does_not_overwrite_finger_changed_after_update_was_deferred() {
    let local = did(0);
    let fresher = did(8);
    let peer = did(16);
    let next = step(
        &state(local, vec![fresher], None, vec![Some(fresher); 5], 0),
        TopologyEvent::Admit {
            peer,
            fixed_fingers: vec![ConditionalFingerUpdate {
                index: 4,
                expected: None,
            }],
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fingers[4], Some(fresher));
}

#[test]
fn test_fix_finger_step_updates_local_successor_slot() {
    let local = did(0);
    let successor = did(8);
    let next = step(
        &state(local, vec![successor], None, vec![None; 4], 2),
        TopologyEvent::FixFinger,
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fix_finger_index, 3);
    assert_eq!(next.state.fingers, vec![None, None, None, Some(successor)]);
    assert!(next.actions.is_empty());
}

#[test]
fn test_fix_finger_step_emits_indexed_remote_action() {
    let local = did(0);
    let successor = did(4);
    let next_hop = did(6);
    let next = step(
        &state(
            local,
            vec![successor],
            None,
            vec![None, None, Some(next_hop), None],
            2,
        ),
        TopologyEvent::FixFinger,
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fix_finger_index, 3);
    assert_eq!(next.actions, vec![TopologyAction::FindSuccessorForFix {
        next: next_hop,
        did: Did::power_of_two(3),
        index: 3
    }]);
}

#[test]
fn test_fix_finger_step_queries_local_relative_probe() {
    let local = did(100);
    let successor = did(104);
    let next_hop = did(106);
    let next = step(
        &state(
            local,
            vec![successor],
            None,
            vec![None, None, Some(next_hop), None],
            2,
        ),
        TopologyEvent::FixFinger,
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fix_finger_index, 3);
    assert_eq!(next.actions, vec![TopologyAction::FindSuccessorForFix {
        next: next_hop,
        did: local + Did::power_of_two(3),
        index: 3
    }]);
}

#[test]
fn test_apply_finger_step_updates_exact_slot() {
    let local = did(0);
    let successor = did(8);
    let next = step(
        &state(local, vec![], None, vec![None; 4], 0),
        TopologyEvent::ApplyFinger {
            index: 2,
            successor,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fingers, vec![None, None, Some(successor), None]);
    assert!(next.actions.is_empty());
}

#[test]
fn test_apply_finger_step_ignores_self_and_out_of_range_slot() {
    let local = did(0);
    let current = state(local, vec![], None, vec![None; 2], 0);
    let self_update = step(
        &current,
        TopologyEvent::ApplyFinger {
            index: 1,
            successor: local,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let out_of_range = step(
        &current,
        TopologyEvent::ApplyFinger {
            index: 9,
            successor: did(9),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(self_update.state, current);
    assert_eq!(out_of_range.state, current);
}

/// A sparse finger table with no hint preceding the target forwards to the
/// successor head, never to the local node.
#[test]
fn test_find_successor_falls_back_to_successor_head_when_no_finger_precedes_target() {
    let local = did(0);
    let head = did(8);
    let far = did(64);
    let current = state(local, vec![head, did(16)], None, vec![None; 8], 0);

    assert_eq!(find_successor(&current, far), FindSuccessorStep::Remote {
        next: head,
        did: far
    });
}

/// Law: every `Remote { next, did }` step satisfies `next != n` and
/// `dist(n, next) < dist(n, did)`, over sparse, cleared, and populated tables.
#[test]
fn test_find_successor_remote_hop_always_makes_strict_progress() {
    let local = did(0);
    let head = did(8);
    let tables = [
        vec![None; 6],
        vec![Some(head), Some(head), None, None, None, None],
        vec![None, None, Some(did(16)), Some(did(16)), None, None],
        vec![Some(local), Some(local), Some(local), None, None, None],
        vec![
            Some(head),
            Some(head),
            Some(head),
            Some(head),
            Some(did(16)),
            Some(did(40)),
        ],
    ];

    for fingers in tables {
        let current = state(local, vec![head], None, fingers, 0);
        for target in 1..=64u32 {
            let target = did(target);
            if let FindSuccessorStep::Remote { next, did } = find_successor(&current, target) {
                assert_ne!(next, local, "self hop for target {target}");
                assert!(
                    dist(local, next) < dist(local, did),
                    "no progress toward {target} via {next}"
                );
            }
        }
    }
}

/// A successor entry equal to `local` is representable through the public
/// fields and must be treated as no successor, never as a remote hop.
#[test]
fn test_find_successor_treats_local_successor_entry_as_absent() {
    let local = did(0);
    let only_local = state(local, vec![local], None, vec![Some(local), None], 0);
    assert_eq!(
        find_successor(&only_local, did(8)),
        FindSuccessorStep::Local(local)
    );

    let head = did(4);
    let local_then_head = state(local, vec![local, head], None, vec![Some(local), None], 0);
    assert_eq!(
        find_successor(&local_then_head, did(2)),
        FindSuccessorStep::Local(head)
    );
    assert_eq!(
        find_successor(&local_then_head, did(8)),
        FindSuccessorStep::Remote {
            next: head,
            did: did(8)
        }
    );
}

/// Finger maintenance on a cleared table asks the successor head, not itself.
#[test]
fn test_fix_finger_step_forwards_to_successor_head_when_fingers_are_sparse() {
    let local = did(0);
    let successor = did(4);
    let next = step(
        &state(local, vec![successor], None, vec![None; 4], 2),
        TopologyEvent::FixFinger,
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fix_finger_index, 3);
    assert_eq!(next.actions, vec![TopologyAction::FindSuccessorForFix {
        next: successor,
        did: Did::power_of_two(3),
        index: 3
    }]);
}

#[test]
fn test_referenced_peers_collect_every_topology_slot_except_local() {
    let local = did(0);
    let successor = did(8);
    let predecessor = did(200);
    let finger = did(32);
    let current = state(
        local,
        vec![successor, local],
        Some(predecessor),
        vec![Some(finger), None, Some(local), Some(successor)],
        0,
    );

    assert_eq!(
        current.referenced_peers(),
        BTreeSet::from([successor, finger, predecessor])
    );
    for peer in [successor, predecessor, finger] {
        assert!(current.references(peer));
    }
    assert!(!current.references(local));
    assert!(!current.references(did(1)));
}

#[test]
fn test_successor_head_skips_local_and_is_absent_when_alone() {
    let local = did(0);
    assert_eq!(
        successor_head(&state(local, vec![local, did(20)], None, vec![None; 5], 0)),
        Some(did(20))
    );
    assert_eq!(
        successor_head(&state(local, vec![local], None, vec![None; 5], 0)),
        None
    );
}

/// The head law: `SuccessorHeadChanged(h)` is emitted, last and once, iff the head moved to `h`.
fn assert_head_law(before: &TopologyState, next: &TopologyStep) {
    let head_changes = next
        .actions
        .iter()
        .filter(|action| matches!(action, TopologyAction::SuccessorHeadChanged(_)))
        .count();
    match successor_head(&next.state).filter(|head| successor_head(before) != Some(*head)) {
        Some(head) => {
            assert_eq!(head_changes, 1);
            assert_eq!(
                next.actions.last(),
                Some(&TopologyAction::SuccessorHeadChanged(head))
            );
        }
        None => assert_eq!(head_changes, 0),
    }
}

#[test]
fn test_admit_step_reports_head_change_only_when_the_head_moves() {
    let local = did(0);
    let current = state(local, vec![did(30)], None, vec![None; 5], 0);

    let closer = step(
        &current,
        TopologyEvent::Admit {
            peer: did(20),
            fixed_fingers: Vec::new(),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_head_law(&current, &closer);
    assert_eq!(
        closer.actions.last(),
        Some(&TopologyAction::SuccessorHeadChanged(did(20)))
    );

    let farther = step(
        &current,
        TopologyEvent::Admit {
            peer: did(40),
            fixed_fingers: Vec::new(),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_head_law(&current, &farther);
    assert!(!farther
        .actions
        .iter()
        .any(|action| matches!(action, TopologyAction::SuccessorHeadChanged(_))));
}

#[test]
fn test_stabilize_step_reports_head_change_when_reported_predecessor_precedes_head() {
    let local = did(0);
    let current = state(local, vec![did(30)], None, vec![None; 5], 0);
    let next = step(
        &current,
        TopologyEvent::Stabilize {
            successors: vec![did(30), did(40)],
            predecessor: Some(did(20)),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.successors, vec![did(20), did(30)]);
    assert_head_law(&current, &next);
    assert_eq!(
        next.actions.last(),
        Some(&TopologyAction::SuccessorHeadChanged(did(20)))
    );
}

#[test]
fn test_remove_step_reports_head_change_to_the_surviving_successor() {
    let local = did(0);
    let current = state(local, vec![did(20), did(30)], None, vec![None; 5], 0);
    let next = step(
        &current,
        TopologyEvent::Remove {
            peer: did(20),
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_head_law(&current, &next);
    assert_eq!(next.actions, vec![TopologyAction::SuccessorHeadChanged(
        did(30)
    )]);
}

#[test]
fn test_predecessor_and_finger_steps_never_report_a_head_change() {
    let local = did(0);
    let current = state(local, vec![did(30)], None, vec![None; 5], 0);
    for event in [
        TopologyEvent::Notify {
            predecessor: did(90),
        },
        TopologyEvent::FixFinger,
        TopologyEvent::ApplyFinger {
            index: 2,
            successor: did(40),
        },
        TopologyEvent::UpdateSuccessor { successor: did(30) },
    ] {
        let next = step(&current, event, DEFAULT_SUCCESSOR_CAPACITY);
        assert_head_law(&current, &next);
        assert!(!next
            .actions
            .iter()
            .any(|action| matches!(action, TopologyAction::SuccessorHeadChanged(_))));
    }
}

#[test]
fn test_responsibility_is_the_predecessor_interval_or_standing_alone() {
    let local = did(10);
    let with_predecessor = state(local, vec![did(20)], Some(did(5)), vec![None; 5], 0);
    assert!(is_responsible_for(&with_predecessor, did(10)));
    assert!(is_responsible_for(&with_predecessor, did(7)));
    assert!(!is_responsible_for(&with_predecessor, did(5)));
    assert!(!is_responsible_for(&with_predecessor, did(15)));

    let uninformed = state(local, vec![did(20)], None, vec![None; 5], 0);
    assert!(!is_responsible_for(&uninformed, did(7)));

    let alone = state(local, Vec::new(), None, vec![None; 5], 0);
    assert!(is_responsible_for(&alone, did(7)));
    assert!(is_responsible_for(&alone, did(15)));
}

/// Law: a node is never its own predecessor. A candidate equal to `local` leaves the current
/// value, whether one is known or not, so `(pred, local]` is never emptied by a self-reference.
#[test]
fn test_rectify_never_adopts_the_local_node_as_predecessor() {
    let local = did(10);
    assert_eq!(rectify_predecessor(local, None, local), None);
    assert_eq!(
        rectify_predecessor(local, Some(did(5)), local),
        Some(did(5))
    );
    assert_eq!(rectify_predecessor(local, None, did(5)), Some(did(5)));

    let notified_by_itself = step(
        &state(local, vec![did(20)], Some(did(5)), vec![None; 5], 0),
        TopologyEvent::Notify { predecessor: local },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(notified_by_itself.state.predecessor, Some(did(5)));
    assert!(is_responsible_for(&notified_by_itself.state, did(7)));
}
