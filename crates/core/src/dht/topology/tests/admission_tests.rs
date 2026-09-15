//! Regression tests for finger proofs that wait on transport admission.
//!
//! These cases separate proof ownership from handshake timing and verify that
//! only matching admission, cancellation, or timeout can consume retained evidence.

use super::*;
use crate::dht::finger::FingerConvergencePhase;
use crate::dht::finger::FingerReportRejection;
use crate::dht::finger::FingerRetireOutcome;
use crate::dht::finger::FINGER_ADMISSION_TIMEOUT_MS;

/// Prove a timely proof remains valid throughout a long transport handshake.
///
/// Deferral transfers ownership to admission, whose matching atomic transition may
/// commit after the original lookup deadline has elapsed.
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

    // Once deferred, the proof is owned by the admission lease rather than the
    // original lookup deadline: it survives until the last millisecond of the
    // lease that started when it was deferred.
    let lease_expiry_ms = 1_001 + FINGER_ADMISSION_TIMEOUT_MS;
    let after_original_lookup_deadline = step(
        &deferred.state,
        advance(lease_expiry_ms - 2),
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
            now_ms: lease_expiry_ms - 1,
        },
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    assert_eq!(admitted.state.fingers[3], Some(candidate));
    let projection = admitted.state.finger_convergence_projection();
    assert_eq!(projection.deferred, None);
    assert_eq!(projection.failure_streak, 0);
}

/// Prove a conflicting duplicate cannot replace a retained admission proof.
///
/// Once the first candidate owns the token, another successor using that request
/// is rejected while the legitimate candidate remains able to commit.
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

    // A second report with the same token but a different candidate is stale
    // and must not evict the first candidate's retained proof.
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

/// Prove an unroutable duplicate cannot retire the current admission owner.
///
/// Conflicting evidence is stale by correlation and leaves the original deferred
/// request available for legitimate completion.
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

    // Retiring the wrong candidate is treated as stale; only the owner of the
    // deferred admission lease can consume it.
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

/// Prove an abandoned deferred proof expires into bounded retry backoff.
///
/// Reaching the admission lease boundary clears ownership, increments failure
/// history, and schedules the next eligible convergence turn.
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

    let lease_expiry_ms = 1_001 + FINGER_ADMISSION_TIMEOUT_MS;
    let expired = step(
        &deferred.state,
        advance(lease_expiry_ms),
        DEFAULT_SUCCESSOR_CAPACITY,
    );
    let projection = expired.state.finger_convergence_projection();

    assert!(expired.actions.is_empty());
    assert_eq!(projection.deferred, None);
    assert_eq!(projection.failure_streak, 1);
    assert_eq!(
        projection.retry_not_before_ms,
        Some(lease_expiry_ms + 2_000)
    );
}

/// Prove consecutive deferred-handshake failures accumulate exponential backoff.
///
/// Each proof is deferred and cancelled independently; the second failure must
/// retain the first failure's history instead of restarting at zero.
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

    // Two admission failures for the same slot should grow the same retry
    // counter used by direct lookup send/report failures.
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

/// Prove cancellation releases a deferred proof and applies failure backoff.
///
/// The matching token relinquishes admission ownership but cannot issue another
/// lookup until the production delay has elapsed.
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
