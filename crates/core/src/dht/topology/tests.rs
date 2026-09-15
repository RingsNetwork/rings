//! Unit tests for pure topology transitions and local Chord lookup laws.

use std::collections::BTreeSet;

use num_bigint::BigUint;

use super::*;

/// Compact DID fixture for small integer rings.
fn did(value: u32) -> Did {
    Did::from(value)
}

/// Convert an integer into a deterministic topology request UUID.
///
/// Distinct values make correlation and supersession assertions reproducible.
fn request_id(value: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(value)
}

/// Build a convergence tick with deterministic time and identity inputs.
///
/// Coupling the UUID to `now_ms` prevents accidental token reuse in clock tests.
fn advance(now_ms: u64) -> TopologyEvent {
    TopologyEvent::AdvanceFingerConvergence {
        now_ms,
        request_id: request_id(u128::from(now_ms)),
    }
}

/// Build a topology state with fresh finger-convergence metadata.
fn state(
    local: Did,
    successors: Vec<Did>,
    predecessor: Option<Did>,
    fingers: Vec<Option<Did>>,
    fix_finger_index: usize,
) -> TopologyState {
    TopologyState::new(local, successors, predecessor, fingers, fix_finger_index)
}

/// Prepare one artificial in-flight finger lookup for a selected slot.
///
/// All other slots are marked verified so production selection must issue the
/// requested slot and provide a valid owner token for report tests.
/// The exact request token for `slot`; every test slot fits the wire width.
fn fix_request(slot: usize, request_id: uuid::Uuid) -> FingerFixRequest {
    FingerFixRequest::new(slot, request_id).expect("test slot fits the u16 wire width")
}

fn issue_request(current: &mut TopologyState, slot: usize, now_ms: u64) -> FingerFixRequest {
    // Mark every other slot verified so the requested slot is the only eligible
    // lookup target for this fixture.
    current.finger_convergence.fill_verified_for_test(true);
    assert!(current
        .finger_convergence
        .set_slot_verified_for_test(slot, false));
    let request = current.finger_convergence.prepare_lookup(
        &current.fingers,
        0,
        now_ms,
        request_id(u128::from(now_ms)),
    );
    request.expect("test request must be issued")
}

/// Successor distance vector padded with infinity for missing successor slots.
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

/// Whether `after` is component-wise no farther than `before`.
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

/// Apply one claimed stabilization report from the current head: the round is
/// begun and claimed with one token, and the report carries that token.
fn claimed_stabilize(
    current: &TopologyState,
    successors: Vec<Did>,
    predecessor: Option<Did>,
) -> TopologyStep {
    let reporter = successor_head(current).expect("stabilization needs a head");
    let request_id = uuid::Uuid::from_u128(1);
    let begun = step(
        current,
        TopologyEvent::BeginStabilize { request_id },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let claimed = step(
        &begun,
        TopologyEvent::ClaimStabilize {
            reporter,
            request_id,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    step(
        &claimed,
        TopologyEvent::Stabilize {
            reporter,
            request_id,
            successors,
            predecessor,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
}

/// Verifies that stabilization replaces a farther successor with a reported
/// predecessor and strictly refines the clockwise successor-distance vector.
#[test]
fn test_stabilize_step_refines_successor_distance_vector() {
    let local = did(0);
    let current = state(local, vec![did(40)], None, vec![None; 5], 0);
    let next = claimed_stabilize(&current, vec![did(50), did(60)], Some(did(10)));

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

/// Verifies that one admission transition atomically joins the peer, applies
/// its retained finger proof, and emits the required topology actions.
#[test]
fn test_admit_step_commits_join_and_pending_fingers_in_one_state() {
    let local = did(0);
    let peer = did(16);
    let mut current = state(local, Vec::new(), None, vec![None; 5], 0);
    let request = issue_request(&mut current, 4, 1_000);
    let next = step(
        &current,
        TopologyEvent::Admit {
            peer,
            deferred_proof: Some(request),
            now_ms: 1_100,
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

/// Verifies that admitting a deferred proof cannot overwrite a finger hint
/// whose evidence epoch advanced after the proof was retained.
#[test]
fn test_admit_step_does_not_overwrite_finger_changed_after_update_was_deferred() {
    let local = did(0);
    let fresher = did(20);
    let peer = did(32);
    let mut current = state(local, vec![peer], None, vec![Some(peer); 5], 0);
    let request = issue_request(&mut current, 4, 1_000);
    let deferred = step(
        &current,
        TopologyEvent::DeferFinger {
            request,
            successor: peer,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    // Joining `fresher` invalidates the deferred proof before admission replays it.
    let changed = step(
        &deferred.state,
        TopologyEvent::Join { peer: fresher },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(changed.state.finger_convergence_projection().deferred, None);
    assert_eq!(
        changed.state.finger_convergence_projection().failure_streak,
        0
    );
    let next = step(
        &changed.state,
        TopologyEvent::Admit {
            peer,
            deferred_proof: Some(request),
            now_ms: 1_100,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fingers[4], Some(fresher));
}

/// Prove an isolated sparse table stays pending without emitting an invalid route.
///
/// With no successor witness, the reducer preserves its cursor and unknown slots
/// while remaining dormant until topology evidence arrives.
#[test]
fn test_fix_finger_step_keeps_isolated_sparse_range_unverified_and_dormant() {
    let local = did(0);
    let next = step(
        &state(local, Vec::new(), None, vec![None; 4], 2),
        advance(1_000),
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fix_finger_index, 2);
    assert_eq!(next.state.fingers, vec![None; 4]);
    assert!(next.state.finger_convergence_pending());
    assert!(!next.state.finger_convergence_status(0).may_advance());
    assert!(next.actions.is_empty());
}

/// Prove a remote finger action carries the selected range's correlation token.
///
/// The only unverified slot determines the lower-bound DID, next hop, and exact
/// request identity required to validate the eventual report.
#[test]
fn test_fix_finger_step_emits_correlated_remote_action() {
    let local = did(0);
    let successor = did(4);
    let next_hop = did(6);
    let mut current = state(
        local,
        vec![successor],
        None,
        vec![None, None, Some(next_hop), None],
        2,
    );
    current
        .finger_convergence
        .set_verified_for_test(&[true, true, true, false]);
    let next = step(&current, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);

    assert_eq!(next.actions, vec![TopologyAction::FindSuccessorForFix {
        next: next_hop,
        did: Did::power_of_two(3),
        request: fix_request(3, request_id(1_000))
    }]);
}

/// Verifies that a finger lookup target is computed relative to the local DID
/// and routed through the current next hop with its correlation token intact.
#[test]
fn test_fix_finger_step_queries_local_relative_probe() {
    let local = did(100);
    let successor = did(104);
    let next_hop = did(106);
    let mut current = state(
        local,
        vec![successor],
        None,
        vec![None, None, Some(next_hop), None],
        2,
    );
    current
        .finger_convergence
        .set_verified_for_test(&[true, true, true, false]);
    let next = step(&current, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);

    assert_eq!(next.actions, vec![TopologyAction::FindSuccessorForFix {
        next: next_hop,
        did: local + Did::power_of_two(3),
        request: fix_request(3, request_id(1_000))
    }]);
}

/// Prove one valid distance proof updates every covered finger slot.
///
/// A successor for slot two also proves slot three, witnessing range application
/// instead of one-result-per-slot mutation.
#[test]
fn test_apply_finger_step_updates_every_slot_proved_by_distance() {
    let local = did(0);
    let successor = did(8);
    let mut current = state(local, vec![], None, vec![None; 4], 0);
    let request = issue_request(&mut current, 2, 1_000);
    let next = step(
        &current,
        TopologyEvent::ApplyFinger {
            request,
            successor,
            now_ms: 1_100,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(next.state.fingers, vec![
        None,
        None,
        Some(successor),
        Some(successor)
    ]);
    assert_eq!(next.state.fix_finger_index, 3);
    assert!(next.actions.is_empty());
}

/// Prove invalid, replayed, and out-of-range reports cannot mutate finger slots.
///
/// Insufficient correlated evidence enters backoff; its replay and an impossible
/// slot are then rejected without additional state change.
#[test]
fn test_apply_finger_step_rejects_stale_and_invalid_results() {
    let local = did(0);
    let mut current = state(local, vec![], None, vec![None; 4], 0);
    let request = issue_request(&mut current, 3, 1_000);
    let invalid = step(
        &current,
        TopologyEvent::ApplyFinger {
            request,
            successor: did(4),
            now_ms: 1_100,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let stale = step(
        &invalid.state,
        TopologyEvent::ApplyFinger {
            request,
            successor: did(8),
            now_ms: 1_200,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let out_of_range = fix_request(9, request_id(2));
    let ignored = step(
        &current,
        TopologyEvent::ApplyFinger {
            request: out_of_range,
            successor: did(9),
            now_ms: 1_100,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(invalid.state.fingers, current.fingers);
    assert_eq!(
        invalid.state.finger_convergence.status().failure_streak(),
        1
    );
    assert_eq!(stale.state, invalid.state);
    assert_eq!(ignored.state, current);
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

/// Prove a local-successor range waits for authenticated stabilization evidence.
///
/// The convergence tick emits no route around the ring; only the claimed current
/// head report may verify locally covered slots.
#[test]
fn test_local_successor_range_waits_for_stabilization_instead_of_routing_around_ring() {
    let local = did(0);
    let head = did(4);
    let mut current = state(
        local,
        vec![head],
        None,
        vec![Some(head), Some(head), Some(head), None],
        0,
    );
    current
        .finger_convergence
        .set_verified_for_test(&[false, false, false, true]);

    assert!(!current.finger_convergence_status(0).may_advance());
    let dormant = step(&current, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    assert!(dormant.actions.is_empty());
    assert_eq!(dormant.state.finger_convergence.verified_for_test(), vec![
        false, false, false, true
    ]);

    // The head's authenticated `pred(head) == local` report proves the local
    // successor range without routing a lookup around the ring.
    let request_id = request_id(2_000);
    let begun = step(
        &dormant.state,
        TopologyEvent::BeginStabilize { request_id },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let claimed = step(
        &begun.state,
        TopologyEvent::ClaimStabilize {
            reporter: head,
            request_id,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let stabilized = step(
        &claimed.state,
        TopologyEvent::Stabilize {
            reporter: head,
            request_id,
            successors: Vec::new(),
            predecessor: Some(local),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(stabilized
        .actions
        .iter()
        .all(|action| !matches!(action, TopologyAction::FindSuccessorForFix { .. })));
    assert_eq!(
        stabilized.state.finger_convergence.verified_for_test(),
        vec![true, true, true, true]
    );
}

/// Prove a superseded stabilization token cannot verify the local successor range.
///
/// The stale report preserves evidence, while claiming and consuming the current
/// token verifies the same slots and isolates correlation as the decisive input.
#[test]
fn test_stale_stabilization_report_cannot_verify_the_local_successor_range() {
    let local = did(0);
    let head = did(4);
    let mut current = state(
        local,
        vec![head],
        None,
        vec![Some(head), Some(head), Some(head), None],
        0,
    );
    current
        .finger_convergence
        .set_verified_for_test(&[false, false, false, true]);
    let old_request = request_id(10);
    let current_request = request_id(11);
    let first = step(
        &current,
        TopologyEvent::BeginStabilize {
            request_id: old_request,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let superseded = step(
        &first.state,
        TopologyEvent::BeginStabilize {
            request_id: current_request,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let stale = step(
        &superseded.state,
        TopologyEvent::Stabilize {
            reporter: head,
            request_id: old_request,
            successors: Vec::new(),
            predecessor: Some(local),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(stale.state, superseded.state);
    assert_eq!(stale.state.finger_convergence.verified_for_test(), vec![
        false, false, false, true
    ]);

    let claimed = step(
        &stale.state,
        TopologyEvent::ClaimStabilize {
            reporter: head,
            request_id: current_request,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let fresh = step(
        &claimed.state,
        TopologyEvent::Stabilize {
            reporter: head,
            request_id: current_request,
            successors: Vec::new(),
            predecessor: Some(local),
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(fresh.state.finger_convergence.verified_for_test(), vec![
        true, true, true, true
    ]);
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
    let mut current = state(local, vec![successor], None, vec![None; 4], 2);
    current
        .finger_convergence
        .set_verified_for_test(&[true, true, true, false]);
    let next = step(&current, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);

    assert_eq!(next.actions, vec![TopologyAction::FindSuccessorForFix {
        next: successor,
        did: Did::power_of_two(3),
        request: fix_request(3, request_id(1_000))
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

/// Verifies that admission emits `SuccessorHeadChanged` exactly when the
/// admitted peer becomes the new closest clockwise successor.
#[test]
fn test_admit_step_reports_head_change_only_when_the_head_moves() {
    let local = did(0);
    let current = state(local, vec![did(30)], None, vec![None; 5], 0);

    let closer = step(
        &current,
        TopologyEvent::Admit {
            peer: did(20),
            deferred_proof: None,
            now_ms: 1_000,
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
            deferred_proof: None,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_head_law(&current, &farther);
    assert!(!farther
        .actions
        .iter()
        .any(|action| matches!(action, TopologyAction::SuccessorHeadChanged(_))));
}

/// Verifies that a stabilization report announces a head change when the
/// reporter's predecessor lies before the previous successor head.
#[test]
fn test_stabilize_step_reports_head_change_when_reported_predecessor_precedes_head() {
    let local = did(0);
    let current = state(local, vec![did(30)], None, vec![None; 5], 0);
    let next = claimed_stabilize(&current, vec![did(30), did(40)], Some(did(20)));

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

/// Verifies that predecessor notification and finger-maintenance transitions
/// never emit a successor-head change for an unchanged successor set.
#[test]
fn test_predecessor_and_finger_steps_never_report_a_head_change() {
    let local = did(0);
    let current = state(local, vec![did(30)], None, vec![None; 5], 0);
    let request = fix_request(2, request_id(1));
    for event in [
        TopologyEvent::Notify {
            predecessor: did(90),
        },
        TopologyEvent::BeginFingerRevalidation,
        TopologyEvent::ApplyFinger {
            request,
            successor: did(40),
            now_ms: 1_000,
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

/// Admission-focused regression tests for deferred finger proofs.
///
/// The submodule keeps timeout, duplicate, and supersession cases close to the
/// topology reducer while separating them from the broader ring-shape tests in
/// this file.
mod admission_tests;
