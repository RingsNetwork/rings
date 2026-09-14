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

fn converge_with_oracle(mut current: TopologyState, all: &[Did]) -> (TopologyState, usize) {
    let mut lookups = 0usize;
    for round in 0..=RING_BITS {
        if !current.finger_convergence_pending() {
            return (current, lookups);
        }
        let now_ms = u64::try_from(round)
            .unwrap_or(u64::MAX)
            .saturating_add(1)
            .saturating_mul(FINGER_LOOKUP_MIN_INTERVAL_MS);
        let advanced = step(
            &current,
            TopologyEvent::AdvanceFingerConvergence { now_ms },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let request = match advanced.actions.as_slice() {
            [TopologyAction::FindSuccessorForFix { request, .. }] => Some(*request),
            [] => None,
            actions => {
                assert!(
                    actions.is_empty(),
                    "unexpected convergence actions: {actions:?}"
                );
                None
            }
        };
        current = advanced.state;
        if let Some(request) = request {
            lookups = lookups.saturating_add(1);
            let successor =
                finger(all, current.local, request.slot_index()).unwrap_or(current.local);
            current = step(
                &current,
                TopologyEvent::ApplyFinger {
                    request,
                    successor,
                    now_ms,
                },
                DEFAULT_SUCCESSOR_CAPACITY,
            )
            .state;
        }
    }
    assert!(
        !current.finger_convergence_pending(),
        "finger convergence exceeded the fixed table width"
    );
    (current, lookups)
}

fn distinct_ranges(fingers: &[Option<Did>]) -> usize {
    fingers
        .iter()
        .enumerate()
        .filter(|(index, value)| {
            index
                .checked_sub(1)
                .and_then(|previous| fingers.get(previous))
                != Some(value)
        })
        .count()
}

#[test]
fn test_five_node_bootstrap_converges_in_two_proved_range_lookups() {
    let local = Did::from(BigUint::from(0u8));
    let lower = (BigUint::from(1u8) << 159) - BigUint::from(1u8);
    let upper = BigUint::from(1u8) << 159;
    let seed = Did::from(lower);
    let all = vec![
        local,
        seed,
        Did::from(&upper + BigUint::from(100u16)),
        Did::from(&upper + BigUint::from(200u16)),
        Did::from(&upper + BigUint::from(300u16)),
    ];
    let initial = step(
        &state(local, Vec::new(), None, vec![None; RING_BITS], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;

    let (converged, lookups) = converge_with_oracle(initial, &all);
    let expected = finger_table(&all, local);

    assert_eq!(converged.fingers, expected);
    assert_eq!(distinct_ranges(&expected), 2);
    assert_eq!(lookups, 2);
}

#[test]
fn test_dense_ring_converges_with_at_most_one_remote_lookup_per_range() {
    let local = did(0);
    let peers = (0..RING_BITS)
        .map(|bit| Did::from(BigUint::from(1u8) << bit))
        .collect::<Vec<_>>();
    let mut all = vec![local];
    all.extend(peers.iter().copied());
    let seed = peers.last().copied().unwrap_or(local);
    let initial = step(
        &state(local, Vec::new(), None, vec![None; RING_BITS], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;

    let (converged, lookups) = converge_with_oracle(initial, &all);
    let expected = finger_table(&all, local);

    assert_eq!(converged.fingers, expected);
    // The seed is exactly the final range target, so that range is proved
    // locally; every other distinct range requires exactly one remote lookup.
    assert_eq!(lookups, distinct_ranges(&expected).saturating_sub(1));
}

#[test]
fn test_insertion_and_removal_invalidate_only_changed_finger_slots() {
    let local = did(0);
    let seed = did(64);
    let closer = did(16);
    let initial = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (stable, _) = converge_with_oracle(initial, &[local, seed]);

    let inserted = step(
        &stable,
        TopologyEvent::Join { peer: closer },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(inserted.finger_convergence.verified, vec![
        false, false, false, false, false, true, true, true
    ]);
    let (with_closer, _) = converge_with_oracle(inserted, &[local, closer, seed]);
    assert_eq!(
        with_closer.fingers,
        expected_fingers(&[local, closer, seed], local, 8)
    );

    let removed = step(
        &with_closer,
        TopologyEvent::Remove {
            peer: closer,
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(removed.finger_convergence.verified, vec![
        false, false, false, false, false, true, true, true
    ]);
    let (without_closer, _) = converge_with_oracle(removed, &[local, seed]);
    assert_eq!(
        without_closer.fingers,
        expected_fingers(&[local, seed], local, 8)
    );
}

fn expected_fingers(all: &[Did], local: Did, slot_count: usize) -> Vec<Option<Did>> {
    let mut expected = finger_table(all, local);
    expected.truncate(slot_count);
    expected
}

fn oracle_state(all: &[Did], local: Did, slot_count: usize) -> TopologyState {
    state(
        local,
        successors(all, local, DEFAULT_SUCCESSOR_CAPACITY),
        predecessor(all, local),
        expected_fingers(all, local, slot_count),
        0,
    )
}

#[test]
fn test_sparse_and_dense_mutation_matrix_converges_to_finger_table_oracle() {
    let local = did(0);
    let sparse = vec![local, did(3), did(63), did(200)];
    let mut dense = vec![local];
    dense.extend((0..8).map(|bit| Did::from(BigUint::from(1u8) << bit)));

    for initial_members in [sparse, dense] {
        let (stable, _) =
            converge_with_oracle(oracle_state(&initial_members, local, 8), &initial_members);
        assert_eq!(stable.fingers, expected_fingers(&initial_members, local, 8));

        for inserted in [did(5), did(33), did(129)] {
            if initial_members.contains(&inserted) {
                continue;
            }
            let mut members = initial_members.clone();
            members.push(inserted);
            let joined = step(
                &stable,
                TopologyEvent::Join { peer: inserted },
                DEFAULT_SUCCESSOR_CAPACITY,
            )
            .state;
            let (converged, _) = converge_with_oracle(joined, &members);
            assert_eq!(converged.fingers, expected_fingers(&members, local, 8));
        }

        for removed in initial_members
            .iter()
            .copied()
            .filter(|peer| *peer != local)
        {
            let members = initial_members
                .iter()
                .copied()
                .filter(|peer| *peer != removed)
                .collect::<Vec<_>>();
            let without_peer = step(
                &stable,
                TopologyEvent::Remove {
                    peer: removed,
                    successor: SuccessorRemoval::Preserve,
                },
                DEFAULT_SUCCESSOR_CAPACITY,
            )
            .state;
            let (converged, _) = converge_with_oracle(without_peer, &members);
            assert_eq!(converged.fingers, expected_fingers(&members, local, 8));
        }
    }
}

#[test]
fn test_topology_change_rejects_an_in_flight_stale_range_result() {
    let local = did(0);
    let seed = did(128);
    let closer = did(8);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(
        &hinted,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 1_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let request = match issued.actions.as_slice() {
        [TopologyAction::FindSuccessorForFix { request, .. }] => *request,
        actions => {
            assert!(
                actions.is_empty(),
                "expected one lookup action, got {actions:?}"
            );
            FingerFixRequest {
                slot: u16::MAX,
                request_id: u64::MAX,
            }
        }
    };
    let changed = step(
        &issued.state,
        TopologyEvent::Join { peer: closer },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(changed.finger_convergence.status().failure_streak(), 1);
    let early_retry = step(
        &changed,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 2_999 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(early_retry.actions.is_empty());
    let stale = step(
        &changed,
        TopologyEvent::ApplyFinger {
            request,
            successor: seed,
            now_ms: 1_500,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;

    assert_eq!(stale, changed);
    assert_eq!(stale.fingers.first().copied().flatten(), Some(closer));
}

#[test]
fn test_finger_lookup_has_one_in_flight_request_and_a_bounded_retry() {
    let local = did(0);
    let seed = did(128);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(
        &hinted,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 1_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let first = issued.actions.iter().find_map(|action| match action {
        TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
        _ => None,
    });
    assert!(first.is_some());

    let blocked = step(
        &issued.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 10_999 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(blocked.actions.is_empty());

    let expired = step(
        &blocked.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 11_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(expired.actions.is_empty());
    assert_eq!(
        expired.state.finger_convergence.status().failure_streak(),
        1
    );

    let still_backing_off = step(
        &expired.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 12_999 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(still_backing_off.actions.is_empty());

    let retried = step(
        &still_backing_off.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 13_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let retry = retried.actions.iter().find_map(|action| match action {
        TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
        _ => None,
    });
    assert!(retry.is_some());
    assert_ne!(first, retry);
}

#[test]
fn test_periodic_revalidation_marks_a_range_without_emitting_a_lookup() {
    let local = did(0);
    let seed = did(128);
    let mut stable = state(local, vec![seed], None, vec![Some(seed); 8], 0);
    stable.finger_convergence.verified.fill(true);

    let marked = step(
        &stable,
        TopologyEvent::BeginFingerRevalidation,
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(marked.actions.is_empty());
    assert!(marked.state.finger_convergence_pending());

    let advanced = step(
        &marked.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 1_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(
        advanced
            .actions
            .iter()
            .filter(|action| matches!(action, TopologyAction::FindSuccessorForFix { .. }))
            .count(),
        1
    );
}

#[test]
fn test_cancelled_finger_lookup_still_obeys_the_per_node_rate_limit() {
    let local = did(0);
    let seed = did(128);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(
        &hinted,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 1_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let request = issued.actions.iter().find_map(|action| match action {
        TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
        _ => None,
    });
    let Some(request) = request else {
        assert!(issued.actions.is_empty(), "expected a finger lookup action");
        return;
    };
    let cancelled = step(
        &issued.state,
        TopologyEvent::CancelFinger {
            request,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    let early = step(
        &cancelled.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 2_999 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(early.actions.is_empty());
    let due = step(
        &early.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 3_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(
        due.actions
            .iter()
            .filter(|action| matches!(action, TopologyAction::FindSuccessorForFix { .. }))
            .count(),
        1
    );
}

#[test]
fn test_persistent_send_failures_have_a_capped_exponential_emission_bound() {
    let local = did(0);
    let seed = did(128);
    let mut current = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let mut issued_at_ms = 1_000u64;
    let first = step(
        &current,
        TopologyEvent::AdvanceFingerConvergence {
            now_ms: issued_at_ms,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let mut request = first.actions.iter().find_map(|action| match action {
        TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
        _ => None,
    });
    assert!(request.is_some());
    current = first.state;

    let retry_floors_ms = [2_000u64, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000];
    let mut emission_count = 1usize;
    for (index, retry_floor_ms) in retry_floors_ms.into_iter().enumerate() {
        let Some(current_request) = request else {
            assert!(
                request.is_some(),
                "failure model lost its in-flight request"
            );
            return;
        };
        let cancelled = step(
            &current,
            TopologyEvent::CancelFinger {
                request: current_request,
                now_ms: issued_at_ms,
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        assert_eq!(
            cancelled.state.finger_convergence.status().failure_streak(),
            u8::try_from(index).unwrap_or(u8::MAX).saturating_add(1)
        );

        let retry_at_ms = issued_at_ms.saturating_add(retry_floor_ms);
        let early = step(
            &cancelled.state,
            TopologyEvent::AdvanceFingerConvergence {
                now_ms: retry_at_ms.saturating_sub(1),
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        assert!(early.actions.is_empty());

        let due = step(
            &early.state,
            TopologyEvent::AdvanceFingerConvergence {
                now_ms: retry_at_ms,
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        request = due.actions.iter().find_map(|action| match action {
            TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
            _ => None,
        });
        assert_eq!(
            due.actions
                .iter()
                .filter(|action| matches!(action, TopologyAction::FindSuccessorForFix { .. }))
                .count(),
            1
        );
        emission_count = emission_count.saturating_add(1);
        current = due.state;
        issued_at_ms = retry_at_ms;
    }

    assert_eq!(issued_at_ms, 183_000);
    assert_eq!(emission_count, 8);
}

#[test]
fn test_proved_progress_resets_failure_backoff() {
    let local = did(0);
    let seed = did(128);
    let initial = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let first = step(
        &initial,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 1_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let request = first.actions.iter().find_map(|action| match action {
        TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
        _ => None,
    });
    let Some(request) = request else {
        assert!(first.actions.is_empty(), "expected a finger lookup action");
        return;
    };
    let cancelled = step(
        &first.state,
        TopologyEvent::CancelFinger {
            request,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let retry = step(
        &cancelled.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 3_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let retry_request = retry.actions.iter().find_map(|action| match action {
        TopologyAction::FindSuccessorForFix { request, .. } => Some(*request),
        _ => None,
    });
    let Some(retry_request) = retry_request else {
        assert!(retry.actions.is_empty(), "expected a retry action");
        return;
    };
    let progressed = step(
        &retry.state,
        TopologyEvent::ApplyFinger {
            request: retry_request,
            successor: did(8),
            now_ms: 3_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    assert_eq!(
        progressed
            .state
            .finger_convergence
            .status()
            .failure_streak(),
        0
    );
    let next_range = step(
        &progressed.state,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 4_000 },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(
        next_range
            .actions
            .iter()
            .filter(|action| matches!(action, TopologyAction::FindSuccessorForFix { .. }))
            .count(),
        1
    );
}
