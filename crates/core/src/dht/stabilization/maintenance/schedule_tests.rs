//! Cross-module tests for maintenance scheduling behavior that depends on PeerRing.

use super::*;

/// Default test period with a visible topology/storage phase offset.
const PERIOD: Duration = Duration::from_secs(15);

/// Build a deterministic schedule for one test node.
fn schedule(now_ms: u64, local: crate::dht::Did) -> MaintenanceSchedule {
    MaintenanceSchedule::new(now_ms, PERIOD, local, uuid::Uuid::from_u128(1))
}

/// Compact pending/inactive finger status fixture with no failures.
const fn finger_status(pending: bool) -> FingerConvergenceStatus {
    FingerConvergenceStatus::new(pending, 0)
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_peer_ring_reuses_finger_jitter_entropy_across_listener_restarts() {
    let ring = crate::dht::PeerRing::new_with_storage(
        crate::dht::Did::from(4u32),
        3,
        Box::new(crate::storage::MemStorage::new()),
    );

    assert_eq!(ring.finger_jitter_entropy(), ring.finger_jitter_entropy());
}

#[test]
fn test_long_maintenance_completion_does_not_rephase_a_reserved_finger_turn() {
    let mut schedule = MaintenanceSchedule::new(
        0,
        Duration::from_secs(3_600),
        crate::dht::Did::from(11u32),
        uuid::Uuid::from_u128(1),
    );
    // Force stabilization and finger convergence to be simultaneously due.
    schedule.next_stabilize_ms = 1_000;
    schedule.next_finger_ms = 1_000;
    schedule.finger_phase_last_poll = FingerConvergencePhase::Runnable;

    assert_eq!(
        schedule.poll(1_000, false, finger_status(true)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(!schedule.complete_stabilization(30_000, false));
    assert_eq!(
        schedule.poll(30_000, false, finger_status(true)).task,
        Some(MaintenanceTask::ConvergeFingers)
    );
}

#[test]
fn test_failure_outcome_expands_the_retry_floor_and_full_jitter_window() {
    let mut schedule = schedule(0, crate::dht::Did::from(11u32));
    let _ = schedule.poll(0, false, finger_status(true));

    for failure_streak in [1u8, 2, 3, 4, 5, 6, u8::MAX] {
        schedule.complete_finger_convergence(0, FingerConvergenceStatus::new(true, failure_streak));
        let floor_ms = finger_lookup_backoff_ms(failure_streak);
        assert!((floor_ms..=floor_ms.saturating_mul(2)).contains(&schedule.next_finger_ms));
    }
}

#[test]
fn test_async_failure_rearms_a_not_yet_due_finger_attempt() {
    let mut schedule = schedule(0, crate::dht::Did::from(7u32));
    let _ = schedule.poll(0, false, finger_status(true));
    let initial_deadline = schedule.next_finger_ms;

    assert_eq!(
        schedule
            .poll(500, false, FingerConvergenceStatus::new(true, 1))
            .task,
        None
    );
    assert!((2_500..=4_500).contains(&schedule.next_finger_ms));
    assert_ne!(schedule.next_finger_ms, initial_deadline);
}

#[test]
fn test_capped_failure_jitter_keeps_every_boot_inside_the_retry_window() {
    let local = crate::dht::Did::from(0u32);
    for entropy in 1..=50u128 {
        let mut schedule =
            MaintenanceSchedule::new(0, PERIOD, local, uuid::Uuid::from_u128(entropy));
        let _ = schedule.poll(0, false, FingerConvergenceStatus::new(true, u8::MAX));
        assert!((60_000..=120_000).contains(&schedule.next_finger_ms));
    }
}

#[test]
fn test_finger_convergence_yields_twice_then_gets_a_reserved_turn() {
    let mut schedule = schedule(0, crate::dht::Did::from(0u32));
    // Put the finger deadline on the same tick as stabilization, then on the
    // storage-repair offset, to exercise the two-yield reservation cap.
    schedule.next_finger_ms = 15_000;
    schedule.finger_phase_last_poll = FingerConvergencePhase::Runnable;
    assert_eq!(
        schedule.poll(15_000, false, finger_status(true)).task,
        Some(MaintenanceTask::Stabilize)
    );

    assert!(!schedule.complete_stabilization(15_000, false));
    schedule.next_finger_ms = 20_000;
    assert_eq!(
        schedule.poll(20_000, false, finger_status(true)).task,
        Some(MaintenanceTask::Repair)
    );
    assert_eq!(
        schedule.poll(20_000, true, finger_status(true)).task,
        Some(MaintenanceTask::ConvergeFingers)
    );
    assert_eq!(schedule.finger_priority_deferrals, 0);
}

#[test]
fn test_pending_handshake_stays_dormant_until_lease_expiry() {
    let mut schedule = schedule(0, crate::dht::Did::from(11u32));
    // Admission leases use an absolute expiry from the lookup domain, not a
    // fresh jittered deadline on every scheduler resume.
    let expires_at_ms = 180_000_u64;

    for resumed_at_ms in [0_u64, 60_000, 120_000] {
        let awaiting = FingerConvergenceStatus::awaiting_admission(
            expires_at_ms.saturating_sub(resumed_at_ms),
            0,
        );
        assert_ne!(
            schedule.poll(resumed_at_ms, false, awaiting).task,
            Some(MaintenanceTask::ConvergeFingers)
        );
        assert_eq!(schedule.next_finger_ms, expires_at_ms);
    }
    let expired = FingerConvergenceStatus::awaiting_admission(0, 0);
    assert_eq!(
        schedule.poll(expires_at_ms, false, expired).task,
        Some(MaintenanceTask::Stabilize)
    );
    schedule.complete_stabilization(expires_at_ms, false);
    assert_eq!(
        schedule.poll(expires_at_ms, false, expired).task,
        Some(MaintenanceTask::ConvergeFingers)
    );
}
