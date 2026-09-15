//! End-to-end convergence tests for the pure topology and finger operators.
//!
//! The fixtures compare each production transition with a complete membership
//! oracle while counting routed lookups and exercising retry boundaries.

use num_bigint::BigUint;

use super::*;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerReportRejection;

/// Convert a small integer into the deterministic DID used by compact rings.
///
/// The helper keeps topology examples readable without changing DID ordering.
fn did(value: u32) -> Did {
    Did::from(value)
}

/// Convert an integer into a deterministic request-correlation UUID.
///
/// Distinct inputs model distinct effect-boundary identities without randomness.
fn request_id(value: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(value)
}

/// Build a finger-convergence tick at `now_ms` with a matching deterministic ID.
///
/// Tests needing an identity independent of time construct the event directly.
fn advance(now_ms: u64) -> TopologyEvent {
    TopologyEvent::AdvanceFingerConvergence {
        now_ms,
        request_id: request_id(u128::from(now_ms)),
    }
}

/// Build a topology state from explicit ring views and fresh convergence metadata.
///
/// The production constructor initializes proof, retry, and in-flight state;
/// `cursor` places the slot at which selection and revalidation resume.
fn state(
    local: Did,
    successors: Vec<Did>,
    predecessor: Option<Did>,
    fingers: Vec<Option<Did>>,
    cursor: usize,
) -> TopologyState {
    let mut state = TopologyState::new(local, successors, predecessor, fingers);
    state.finger_convergence.set_cursor_for_test(cursor);
    state
}

/// Extract the only finger lookup request emitted by a topology transition.
///
/// The helper fails when action cardinality or type differs because either shape
/// violates the fixture's single-emission contract.
fn emitted_finger_request(output: &TopologyStep) -> FingerFixRequest {
    match output.actions.as_slice() {
        [TopologyAction::FindSuccessorForFix { request, .. }] => *request,
        actions => panic!("expected exactly one finger lookup action, got {actions:?}"),
    }
}

/// Drive production stabilization and finger lookup transitions to convergence.
///
/// The membership slice supplies only oracle answers; all mutation passes through
/// [`step`]. The returned count witnesses the lookup-per-proved-range bound.
fn converge_with_oracle(mut current: TopologyState, all: &[Did]) -> (TopologyState, usize) {
    let mut lookups = 0usize;
    // A sparse bootstrap seed may first require up to one stabilization head
    // refinement per ring bit before the local successor range can be proved;
    // finger range lookups then require at most one further table width.
    for round in 0..=RING_BITS.saturating_mul(2) {
        if let Some(reporter) = successor_head(&current) {
            // Stabilization uses a fresh token per round so stale reports cannot
            // accidentally prove the local successor range.
            let stabilize_request_id =
                request_id(u128::try_from(round).unwrap_or(u128::MAX).saturating_add(1));
            current = step(
                &current,
                TopologyEvent::BeginStabilize {
                    request_id: stabilize_request_id,
                },
                DEFAULT_SUCCESSOR_CAPACITY,
            )
            .state;
            current = step(
                &current,
                TopologyEvent::ClaimStabilize {
                    reporter,
                    request_id: stabilize_request_id,
                },
                DEFAULT_SUCCESSOR_CAPACITY,
            )
            .state;
            let stabilized = step(
                &current,
                TopologyEvent::Stabilize {
                    reporter,
                    request_id: stabilize_request_id,
                    successors: Vec::new(),
                    predecessor: predecessor(all, reporter),
                },
                DEFAULT_SUCCESSOR_CAPACITY,
            );
            current = stabilized.state;
        }
        if !current.finger_convergence_pending() {
            return (current, lookups);
        }
        // Advance time by the minimum lookup interval to avoid testing the rate
        // limiter instead of topology convergence.
        let now_ms = u64::try_from(round)
            .unwrap_or(u64::MAX)
            .saturating_add(1)
            .saturating_mul(FINGER_LOOKUP_MIN_INTERVAL_MS);
        let advanced = step(&current, advance(now_ms), DEFAULT_SUCCESSOR_CAPACITY);
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
        "finger convergence exceeded the stabilization plus finger-width bound: {:?}",
        current.finger_convergence_projection()
    );
    (current, lookups)
}

/// Count contiguous equal-value runs in a sparse finger table.
///
/// Each run is one range coverable by a successor proof, including unknown slots.
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

/// Prove a five-node bootstrap needs one stabilization proof and one lookup.
///
/// The fixture straddles the highest finger boundary so exactly two ranges expose
/// any redundant routed convergence request.
#[test]
fn test_five_node_bootstrap_uses_one_stabilization_proof_and_one_routed_lookup() {
    let local = Did::from(BigUint::from(0u8));
    // Place the bootstrap seed just below 2^159 and the rest just above it, so
    // stabilization proves the lower half and one routed lookup proves the upper half.
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
    assert_eq!(lookups, 1);
}

/// Prove joining from isolation invalidates every formerly unknown range proof.
///
/// A far peer hidden from the seed must become visible after joining, showing that
/// isolated-state evidence is never reused as verified.
#[test]
fn test_join_after_isolation_revalidates_every_previously_unknown_range() {
    let local = Did::from(BigUint::from(0u8));
    // `lower` is the only initial membership witness; `far` is invisible until
    // convergence revalidates the sparse ranges after joining.
    let lower = (BigUint::from(1u8) << 159) - BigUint::from(1u8);
    let upper = BigUint::from(1u8) << 159;
    let seed = Did::from(lower);
    let far = Did::from(&upper + BigUint::from(7u8));
    let isolated = step(
        &state(local, Vec::new(), None, vec![None; RING_BITS], 0),
        advance(1_000),
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;

    assert!(isolated.finger_convergence_pending());
    assert!(!isolated.finger_convergence_status(0).may_advance());
    assert!(isolated
        .finger_convergence
        .verified_for_test()
        .iter()
        .all(|verified| !verified));

    let joined = step(
        &isolated,
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (converged, lookups) = converge_with_oracle(joined, &[local, seed, far]);

    assert_eq!(converged.fingers, finger_table(&[local, seed, far], local));
    assert_eq!(lookups, 1);
}

/// Prove rejoining after successor loss discards old empty-range evidence.
///
/// The node reconverges against a different ring whose far member requires at
/// least one fresh routed lookup.
#[test]
fn test_finger_rejoin_after_losing_last_successor_discards_old_empty_range_proofs() {
    let local = did(0);
    let old_seed = did(8);
    let new_seed = did(4);
    let far = did(200);
    let joined = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: old_seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (converged, _) = converge_with_oracle(joined, &[local, old_seed]);

    assert!(converged
        .finger_convergence
        .verified_for_test()
        .iter()
        .all(|verified| *verified));
    assert!(converged.fingers.last().is_some_and(Option::is_none));

    let isolated = step(
        &converged,
        TopologyEvent::Remove {
            peer: old_seed,
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert!(isolated.successors.is_empty());
    assert!(isolated
        .finger_convergence
        .verified_for_test()
        .iter()
        .all(|verified| !verified));
    assert!(!isolated.finger_convergence_status(0).may_advance());

    let rejoined = step(
        &isolated,
        TopologyEvent::Join { peer: new_seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (reconverged, lookups) = converge_with_oracle(rejoined, &[local, new_seed, far]);
    let mut expected = finger_table(&[local, new_seed, far], local);
    expected.truncate(reconverged.fingers.len());

    assert_eq!(reconverged.fingers, expected);
    assert!(lookups >= 1);
}

/// Prove restart creates a new request identity and rejects the old report.
///
/// A restarted process rebuilds an empty table and rejoins its seed, exactly
/// as `PeerRing` is constructed; delayed traffic from the previous process
/// then owns nothing in the new one.
#[test]
fn test_restart_uses_a_new_request_identity_and_rejects_the_old_report() {
    let local = did(0);
    let seed = did(1);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let before_restart = step(&hinted, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let old_request = emitted_finger_request(&before_restart);

    let restarted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let after_restart = step(&restarted, advance(2_000), DEFAULT_SUCCESSOR_CAPACITY);
    let new_request = emitted_finger_request(&after_restart);
    assert_ne!(old_request, new_request);

    let stale = step(
        &after_restart.state,
        TopologyEvent::ApplyFinger {
            request: old_request,
            successor: did(8),
            now_ms: 2_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(stale.state, after_restart.state);
    let (_, current_outcome) = apply_finger(&stale.state, new_request, did(8), 2_001);
    assert_eq!(current_outcome, FingerApplyOutcome::Applied { end: 3 });
}

/// Prove a report received exactly at its deadline expires without a timer poll.
///
/// Boundary-time delivery preserves slots, clears ownership, and enters the same
/// bounded backoff as explicit timeout cleanup.
#[test]
fn test_finger_report_at_deadline_expires_without_a_scheduler_poll() {
    let local = did(0);
    let seed = did(1);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(&hinted, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let request = emitted_finger_request(&issued);
    let fingers_before = issued.state.fingers.clone();

    let expired = step(
        &issued.state,
        TopologyEvent::ApplyFinger {
            request,
            successor: seed,
            now_ms: 11_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let projection = expired.state.finger_convergence_projection();

    assert!(expired.actions.is_empty());
    assert_eq!(expired.state.fingers, fingers_before);
    assert_eq!(projection.in_flight, None);
    assert_eq!(projection.failure_streak, 1);
    assert_eq!(projection.retry_not_before_ms, Some(13_000));
    let (_, replay_outcome) = apply_finger(&expired.state, request, seed, 11_000);
    assert_eq!(
        replay_outcome,
        FingerApplyOutcome::Rejected(FingerReportRejection::Stale)
    );
}

/// Prove dense-ring convergence emits at most one lookup per finger range.
///
/// A member at every power-of-two boundary makes redundant requests observable
/// while the final table remains directly comparable with the oracle.
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

/// Prove insertion and removal invalidate only slots whose oracle value changes.
///
/// Unaffected slots retain verified evidence while changed ranges reconverge to
/// the exact membership-derived table.
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
    assert_eq!(inserted.finger_convergence.verified_for_test(), vec![
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
    assert_eq!(removed.finger_convergence.verified_for_test(), vec![
        false, false, false, false, false, true, true, true
    ]);
    let (without_closer, _) = converge_with_oracle(removed, &[local, seed]);
    assert_eq!(
        without_closer.fingers,
        expected_fingers(&[local, seed], local, 8)
    );
}

/// Compute the oracle finger table at the configured fixture width.
///
/// Truncation occurs only after the shared full-ring oracle selects successors.
fn expected_fingers(all: &[Did], local: Did, slot_count: usize) -> Vec<Option<Did>> {
    let mut expected = finger_table(all, local);
    expected.truncate(slot_count);
    expected
}

/// Build a fully converged topology state from the membership oracle.
///
/// Successors, predecessor, and fingers are derived independently of the reducer
/// under test, providing a stable mutation starting point.
fn oracle_state(all: &[Did], local: Did, slot_count: usize) -> TopologyState {
    state(
        local,
        successors(all, local, DEFAULT_SUCCESSOR_CAPACITY),
        predecessor(all, local),
        expected_fingers(all, local, slot_count),
        0,
    )
}

/// Prove sparse and dense mutation matrices converge to the same oracle law.
///
/// Shared broad ranges and one-node-per-slot ranges exercise proof reuse and
/// fine-grained invalidation under identical joins and removals.
#[test]
fn test_sparse_and_dense_mutation_matrix_converges_to_finger_table_oracle() {
    let local = did(0);
    // Run the same insert/remove matrix once on broad sparse ranges and once on
    // one-node-per-slot dense ranges.
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

/// Prove join and removal preserve oracle convergence across the ring wrap point.
///
/// A local DID near `2^160` forces modular successor intervals to cross zero for
/// both insertion and subsequent removal.
#[test]
fn test_wrapped_ring_mutations_converge_to_the_finger_table_oracle() {
    // Start near the end of Z/2^160 so the local successor interval wraps.
    let ring_size = BigUint::from(1u8) << RING_BITS;
    let local = Did::from(&ring_size - BigUint::from(16u8));
    let near = did(1);
    let inserted = did(32);
    let far = Did::from(BigUint::from(1u8) << 159);
    let initial_members = vec![local, near, far];
    let initial = step(
        &state(local, Vec::new(), None, vec![None; RING_BITS], 0),
        TopologyEvent::Join { peer: near },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (stable, _) = converge_with_oracle(initial, &initial_members);

    assert_eq!(stable.fingers, finger_table(&initial_members, local));

    let mut with_inserted = initial_members.clone();
    with_inserted.push(inserted);
    let joined = step(
        &stable,
        TopologyEvent::Join { peer: inserted },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (after_join, _) = converge_with_oracle(joined, &with_inserted);

    assert_eq!(after_join.fingers, finger_table(&with_inserted, local));

    let removed = step(
        &after_join,
        TopologyEvent::Remove {
            peer: inserted,
            successor: SuccessorRemoval::Preserve,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let (after_remove, _) = converge_with_oracle(removed, &initial_members);

    assert_eq!(after_remove.fingers, finger_table(&initial_members, local));
}

/// Prove topology mutation rejects a lookup result issued for the previous view.
///
/// A closer peer invalidates the in-flight range before delivery, so the stale
/// report must preserve the replacement finger and all current state.
#[test]
fn test_topology_change_rejects_an_in_flight_stale_range_result() {
    let local = did(0);
    let seed = did(1);
    let closer = did(2);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(&hinted, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let request = emitted_finger_request(&issued);
    let changed = step(
        &issued.state,
        TopologyEvent::Join { peer: closer },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(changed.finger_convergence.status().failure_streak(), 0);
    let retry = step(&changed, advance(2_000), DEFAULT_SUCCESSOR_CAPACITY);
    assert_eq!(retry.actions.len(), 1);
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
    assert_eq!(stale.fingers.get(1).copied().flatten(), Some(closer));
}

/// Prove only one lookup may be in flight and retry emission is time bounded.
///
/// Requests remain suppressed through expiry and backoff, then exactly one newly
/// correlated lookup is emitted at the eligible boundary.
#[test]
fn test_finger_lookup_has_one_in_flight_request_and_a_bounded_retry() {
    let local = did(0);
    let seed = did(1);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(&hinted, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let first = emitted_finger_request(&issued);

    let blocked = step(&issued.state, advance(10_999), DEFAULT_SUCCESSOR_CAPACITY);
    assert!(blocked.actions.is_empty());

    let expired = step(&blocked.state, advance(11_000), DEFAULT_SUCCESSOR_CAPACITY);
    assert!(expired.actions.is_empty());
    assert_eq!(
        expired.state.finger_convergence.status().failure_streak(),
        1
    );

    let still_backing_off = step(&expired.state, advance(12_999), DEFAULT_SUCCESSOR_CAPACITY);
    assert!(still_backing_off.actions.is_empty());

    let retried = step(
        &still_backing_off.state,
        advance(13_000),
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let retry = emitted_finger_request(&retried);
    assert_ne!(first, retry);
}

/// Prove periodic revalidation verifies a locally covered range without routing.
///
/// A new pass invalidates prior evidence, but the successor head immediately
/// proves its slots and therefore emits no network action.
#[test]
fn test_periodic_revalidation_marks_a_range_without_emitting_a_lookup() {
    let local = did(0);
    let seed = did(1);
    let remote = did(8);
    let mut stable = state(
        local,
        vec![seed],
        None,
        vec![
            Some(seed),
            Some(remote),
            Some(remote),
            Some(remote),
            None,
            None,
            None,
            None,
        ],
        1,
    );
    stable.finger_convergence.fill_verified_for_test(true);

    let marked = step(
        &stable,
        TopologyEvent::BeginFingerRevalidation,
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(marked.actions.is_empty());
    assert!(marked.state.finger_convergence_pending());

    let advanced = step(&marked.state, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    assert_eq!(
        advanced
            .actions
            .iter()
            .filter(|action| matches!(action, TopologyAction::FindSuccessorForFix { .. }))
            .count(),
        1
    );
}

/// Prove that a range whose attempt fails does not starve the other ranges.
///
/// After the attempt for the first remote range is cancelled, the next lookup
/// is issued for the following range rather than for the same slot; once every
/// other range is proved, selection wraps back to the failing one, so it costs
/// one attempt per rotation instead of blocking the table.
#[test]
fn test_failed_range_does_not_starve_the_next_range() {
    let local = did(0);
    let seed = did(1);
    let far = did(8);
    let hinted = [seed, far].into_iter().fold(
        state(local, Vec::new(), None, vec![None; 8], 0),
        |current, peer| {
            step(
                &current,
                TopologyEvent::Join { peer },
                DEFAULT_SUCCESSOR_CAPACITY,
            )
            .state
        },
    );
    // Slot 0 lies in the local successor range; `far` hints slots 1..=3 and
    // slots 4..=7 are unknown: two remote ranges.
    assert_eq!(hinted.fingers, vec![
        Some(seed),
        Some(far),
        Some(far),
        Some(far),
        None,
        None,
        None,
        None
    ]);

    let first = step(&hinted, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let failing = emitted_finger_request(&first);
    assert_eq!(failing.slot_index(), 1);
    let cancelled = step(
        &first.state,
        TopologyEvent::CancelFinger {
            request: failing,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    // The retry goes to the next range, past the whole run of `far` hints.
    let retried = step(&cancelled.state, advance(3_000), DEFAULT_SUCCESSOR_CAPACITY);
    let next_range = emitted_finger_request(&retried);
    assert_eq!(next_range.slot_index(), 4);
    let proved = step(
        &retried.state,
        TopologyEvent::ApplyFinger {
            request: next_range,
            successor: did(200),
            now_ms: 3_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let projection = proved.state.finger_convergence_projection();
    assert_eq!(projection.verified[4..], [true, true, true, true]);
    assert_eq!(projection.failure_streak, 0);

    // Only the failing range is left, so selection wraps back to it.
    let wrapped = step(&proved.state, advance(4_001), DEFAULT_SUCCESSOR_CAPACITY);
    assert_eq!(emitted_finger_request(&wrapped).slot_index(), 1);
}

/// Prove periodic revalidation is deferred by pending work only while that
/// work is succeeding.
///
/// With an unverified remote range and no failure, revalidation waits for the
/// pass to finish. Once the pending range is failing, revalidation reopens the
/// next range anyway, so a slot whose successor never admits cannot freeze the
/// revalidation of every proved range.
#[test]
fn test_revalidation_is_deferred_only_by_succeeding_work() {
    let local = did(0);
    let seed = did(1);
    let far = did(8);
    let mut current = state(
        local,
        vec![seed, far],
        None,
        vec![
            Some(seed),
            Some(far),
            Some(far),
            Some(far),
            Some(did(64)),
            Some(did(64)),
            Some(did(64)),
            Some(did(64)),
        ],
        4,
    );
    current.finger_convergence.fill_verified_for_test(true);
    for slot in 1..=3 {
        assert!(current
            .finger_convergence
            .set_slot_verified_for_test(slot, false));
    }

    let deferred = step(
        &current,
        TopologyEvent::BeginFingerRevalidation,
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(deferred.state.finger_convergence_projection().verified[4]);

    let issued = step(&deferred.state, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let failing = emitted_finger_request(&issued);
    assert_eq!(failing.slot_index(), 1);
    let cancelled = step(
        &issued.state,
        TopologyEvent::CancelFinger {
            request: failing,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(
        cancelled
            .state
            .finger_convergence_projection()
            .failure_streak,
        1
    );

    let reopened = step(
        &cancelled.state,
        TopologyEvent::BeginFingerRevalidation,
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let verified = reopened.state.finger_convergence_projection().verified;
    assert_eq!(verified[4..], [false, false, false, false]);
    assert!(verified[0]);
}

/// Prove cancellation cannot bypass the per-node minimum lookup interval.
///
/// Clearing ownership does not permit immediate re-emission; both the rate floor
/// and cancellation backoff must allow the next request.
#[test]
fn test_cancelled_finger_lookup_still_obeys_the_per_node_rate_limit() {
    let local = did(0);
    let seed = did(1);
    let hinted = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let issued = step(&hinted, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let request = emitted_finger_request(&issued);
    let cancelled = step(
        &issued.state,
        TopologyEvent::CancelFinger {
            request,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    let early = step(&cancelled.state, advance(2_999), DEFAULT_SUCCESSOR_CAPACITY);
    assert!(early.actions.is_empty());
    let due = step(&early.state, advance(3_000), DEFAULT_SUCCESSOR_CAPACITY);
    assert_eq!(
        due.actions
            .iter()
            .filter(|action| matches!(action, TopologyAction::FindSuccessorForFix { .. }))
            .count(),
        1
    );
}

/// Prove persistent send failures obey capped exponential retry scheduling.
///
/// Repeated cancellation models failed dispatch and compares every delay with the
/// production backoff function through saturation.
#[test]
fn test_persistent_send_failures_have_a_capped_exponential_emission_bound() {
    let local = did(0);
    let seed = did(1);
    let mut current = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let mut issued_at_ms = 1_000u64;
    let first = step(&current, advance(issued_at_ms), DEFAULT_SUCCESSOR_CAPACITY);
    let mut request = emitted_finger_request(&first);
    current = first.state;

    let retry_floors_ms = [2_000u64, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000];
    let mut emission_count = 1usize;
    for (index, retry_floor_ms) in retry_floors_ms.into_iter().enumerate() {
        let cancelled = step(
            &current,
            TopologyEvent::CancelFinger {
                request,
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
            advance(retry_at_ms.saturating_sub(1)),
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        assert!(early.actions.is_empty());

        let due = step(
            &early.state,
            advance(retry_at_ms),
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        request = emitted_finger_request(&due);
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

/// Prove a successful range proof resets accumulated failure backoff.
///
/// After a cancelled attempt, a valid retry clears the failure streak so later
/// work follows the normal minimum emission interval.
#[test]
fn test_proved_progress_resets_failure_backoff() {
    let local = did(0);
    let seed = did(1);
    let initial = step(
        &state(local, Vec::new(), None, vec![None; 8], 0),
        TopologyEvent::Join { peer: seed },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let first = step(&initial, advance(1_000), DEFAULT_SUCCESSOR_CAPACITY);
    let request = emitted_finger_request(&first);
    let cancelled = step(
        &first.state,
        TopologyEvent::CancelFinger {
            request,
            now_ms: 1_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let retry = step(&cancelled.state, advance(3_000), DEFAULT_SUCCESSOR_CAPACITY);
    let retry_request = emitted_finger_request(&retry);
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
        advance(4_000),
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
