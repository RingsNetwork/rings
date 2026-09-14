use super::*;
use crate::dht::finger::FingerConvergencePhase;
use crate::dht::finger::FingerReportRejection;
use crate::dht::finger::FingerRetireOutcome;

#[test]
fn test_timely_finger_proof_survives_a_long_handshake_until_atomic_admission() {
    let local = did(0);
    let head = did(1);
    let candidate = did(8);
    let mut current = state(
        local,
        vec![head],
        None,
        vec![Some(head), None, None, None],
        0,
    );
    let request = issue_request(&mut current, 3, 1_000);
    let deferred = step(
        &current,
        TopologyEvent::DeferFinger {
            request,
            successor: candidate,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let projection = deferred.state.finger_convergence_projection();
    assert_eq!(projection.in_flight, None);
    assert_eq!(projection.deferred, Some(request));
    assert_eq!(projection.failure_streak, 0);
    assert!(matches!(
        deferred.state.finger_convergence_status(0).phase(),
        FingerConvergencePhase::AwaitingAdmission { .. }
    ));

    let after_original_lookup_deadline = step(
        &deferred.state,
        advance(180_999),
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert!(after_original_lookup_deadline.actions.is_empty());
    assert_eq!(
        after_original_lookup_deadline
            .state
            .finger_convergence_projection()
            .deferred,
        Some(request)
    );

    let admitted = step(
        &after_original_lookup_deadline.state,
        TopologyEvent::Admit {
            peer: candidate,
            fixed_fingers: vec![ConditionalFingerUpdate { request }],
            now_ms: 181_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(admitted.state.fingers[3], Some(candidate));
    let projection = admitted.state.finger_convergence_projection();
    assert_eq!(projection.deferred, None);
    assert_eq!(projection.failure_streak, 0);
}

#[test]
fn test_conflicting_duplicate_cannot_evict_a_retained_finger_proof() {
    let local = did(0);
    let candidate = did(8);
    let conflicting_candidate = did(16);
    let mut current = state(local, vec![did(1)], None, vec![None; 5], 0);
    let request = issue_request(&mut current, 3, 1_000);
    let deferred = step(
        &current,
        TopologyEvent::DeferFinger {
            request,
            successor: candidate,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    let conflicting = step(
        &deferred.state,
        TopologyEvent::DeferFinger {
            request,
            successor: conflicting_candidate,
            now_ms: 1_002,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(conflicting.state, deferred.state);

    let admitted = step(
        &conflicting.state,
        TopologyEvent::Admit {
            peer: candidate,
            fixed_fingers: vec![ConditionalFingerUpdate { request }],
            now_ms: 1_003,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(admitted.state.fingers[3], Some(candidate));
    assert_eq!(
        admitted.state.finger_convergence_projection().deferred,
        None
    );
}

#[test]
fn test_unroutable_conflicting_duplicate_cannot_retire_admission_owner() {
    let local = did(0);
    let candidate = did(8);
    let conflicting_candidate = did(16);
    let mut current = state(local, vec![did(1)], None, vec![None; 5], 0);
    let request = issue_request(&mut current, 3, 1_000);
    let deferred = step(
        &current,
        TopologyEvent::DeferFinger {
            request,
            successor: candidate,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    let (conflicting, outcome) =
        retire_finger_candidate(&deferred.state, request, conflicting_candidate, 1_002);
    assert_eq!(
        outcome,
        FingerRetireOutcome::Rejected(FingerReportRejection::Stale)
    );
    assert_eq!(conflicting.state, deferred.state);

    let (retired, outcome) = retire_finger_candidate(&conflicting.state, request, candidate, 1_003);
    assert_eq!(outcome, FingerRetireOutcome::Retired);
    let projection = retired.state.finger_convergence_projection();
    assert_eq!(projection.deferred, None);
    assert_eq!(projection.failure_streak, 1);
}

#[test]
fn test_abandoned_deferred_finger_proof_expires_into_backoff() {
    let local = did(0);
    let head = did(1);
    let candidate = did(8);
    let mut current = state(
        local,
        vec![head],
        None,
        vec![Some(head), None, None, None],
        0,
    );
    let request = issue_request(&mut current, 3, 1_000);
    let deferred = step(
        &current,
        TopologyEvent::DeferFinger {
            request,
            successor: candidate,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );

    let expired = step(
        &deferred.state,
        advance(181_001),
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let projection = expired.state.finger_convergence_projection();

    assert!(expired.actions.is_empty());
    assert_eq!(projection.deferred, None);
    assert_eq!(projection.failure_streak, 1);
    assert_eq!(projection.retry_not_before_ms, Some(183_001));
}

#[test]
fn test_repeated_deferred_handshake_failures_accumulate_backoff() {
    let local = did(0);
    let head = did(1);
    let candidate = did(8);
    let mut current = state(
        local,
        vec![head],
        None,
        vec![Some(head), None, None, None],
        0,
    );

    let first_request = issue_request(&mut current, 3, 1_000);
    current = step(
        &current,
        TopologyEvent::DeferFinger {
            request: first_request,
            successor: candidate,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    current = step(
        &current,
        TopologyEvent::CancelFinger {
            request: first_request,
            now_ms: 1_002,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(current.finger_convergence_projection().failure_streak, 1);

    let second_request = issue_request(&mut current, 3, 3_002);
    current = step(
        &current,
        TopologyEvent::DeferFinger {
            request: second_request,
            successor: candidate,
            now_ms: 3_003,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    assert_eq!(current.finger_convergence_projection().failure_streak, 1);
    current = step(
        &current,
        TopologyEvent::CancelFinger {
            request: second_request,
            now_ms: 3_004,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    )
    .state;
    let projection = current.finger_convergence_projection();
    assert_eq!(projection.failure_streak, 2);
    assert_eq!(projection.retry_not_before_ms, Some(7_004));
}

#[test]
fn test_cancelled_handshake_releases_deferred_proof_with_backoff() {
    let local = did(0);
    let head = did(1);
    let candidate = did(8);
    let mut current = state(
        local,
        vec![head],
        None,
        vec![Some(head), None, None, None],
        0,
    );
    let request = issue_request(&mut current, 3, 1_000);
    let deferred = step(
        &current,
        TopologyEvent::DeferFinger {
            request,
            successor: candidate,
            now_ms: 1_001,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let cancelled = step(
        &deferred.state,
        TopologyEvent::CancelFinger {
            request,
            now_ms: 181_000,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let projection = cancelled.state.finger_convergence_projection();
    assert_eq!(projection.deferred, None);
    assert_eq!(projection.failure_streak, 1);
    assert_eq!(projection.retry_not_before_ms, Some(183_000));
}
