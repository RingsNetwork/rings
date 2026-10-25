//! Unit tests for the pure maintenance schedule.

use super::*;

/// Default test period with a visible topology/storage phase offset.
const PERIOD: Duration = Duration::from_secs(15);

/// Build a deterministic schedule for unit tests.
fn schedule(now_ms: u64, interval: Duration, local: crate::dht::Did) -> MaintenanceSchedule {
    MaintenanceSchedule::new(now_ms, interval, local, uuid::Uuid::from_u128(1))
}

/// Compact pending/inactive finger status fixture with no failures.
const fn finger_status(pending: bool) -> FingerConvergenceStatus {
    FingerConvergenceStatus::idle(pending, 0)
}

/// Verifies that stabilization and storage repair occupy distinct offsets
/// inside one maintenance period and repeat without collapsing together.
#[test]
fn test_maintenance_phases_are_staggered_within_each_period() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(14_999, false, finger_status(false)).task,
        None
    );
    assert_eq!(
        schedule.poll(15_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(!schedule.complete_stabilization(15_000, false));
    let repair = schedule.poll(20_000, false, finger_status(false));
    assert!(repair.periodic_repair_due);
    assert_eq!(repair.task, Some(MaintenanceTask::Repair));
    schedule.complete_repair(20_000, true);
    assert_eq!(
        schedule.poll(30_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
}

/// Verifies that a stabilization task finishing after the repair phase
/// preserves repair intent and emits it after the mandatory quiet gap.
#[test]
fn test_repeated_stabilization_overruns_preserve_repair_intent() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(15_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(schedule.complete_stabilization(21_000, false));
    let missed_phase = schedule.poll(21_000, true, finger_status(false));
    assert!(!missed_phase.periodic_repair_due);
    assert_eq!(missed_phase.task, None);
    assert_eq!(
        schedule.poll(21_050, true, finger_status(false)).task,
        Some(MaintenanceTask::Repair)
    );
}

/// Verifies that a long stabilization completion advances to the first
/// future stabilization deadline instead of replaying missed periods.
#[test]
fn test_long_stabilization_skips_missed_stabilization_deadlines() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(15_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(schedule.complete_stabilization(46_000, false));

    assert_eq!(schedule.next_stabilize_ms, 60_000);
    assert_ne!(
        schedule.poll(46_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
}

/// Verifies that an overdue repair receives a complete execution window
/// before the next stabilization turn is allowed to start.
#[test]
fn test_stabilization_reserves_a_window_for_pending_repair() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(15_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(schedule.complete_stabilization(26_000, false));
    let reserved_stabilization_ms = schedule.next_stabilize_ms;
    assert!(schedule.has_storage_repair_window(schedule.repair_not_before_ms));
    assert_eq!(schedule.poll(26_000, true, finger_status(false)).task, None);
    assert_eq!(
        schedule.poll(26_050, true, finger_status(false)).task,
        Some(MaintenanceTask::Repair)
    );
    let quiet_gap_ms = duration_ms(MAINTENANCE_QUIET_GAP);
    let repair_completed_ms = reserved_stabilization_ms.saturating_sub(quiet_gap_ms);
    schedule.complete_repair(repair_completed_ms, true);
    assert_eq!(
        schedule
            .poll(repair_completed_ms, false, finger_status(false))
            .task,
        None
    );
    assert_eq!(
        schedule
            .poll(reserved_stabilization_ms, false, finger_status(false))
            .task,
        Some(MaintenanceTask::Stabilize)
    );
}

/// Verifies that waking after a reserved repair window still runs repair
/// rather than discarding its turn because the timer overshot the deadline.
#[test]
fn test_reserved_repair_turn_survives_timer_overshoot() {
    let mut schedule = schedule(0, Duration::from_millis(100), crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(100, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    schedule.complete_stabilization(100, true);
    let first_missed_deadline = schedule
        .repair_not_before_ms
        .saturating_add(schedule.required_repair_window_ms())
        .saturating_add(1);

    assert_eq!(first_missed_deadline, schedule.next_stabilize_ms + 1);
    assert_eq!(
        schedule
            .poll(first_missed_deadline, true, finger_status(false))
            .task,
        Some(MaintenanceTask::Repair)
    );
}

/// Verifies over several late wakeups that repair and stabilization both
/// continue to receive turns and neither phase starves the other.
#[test]
fn test_repeated_timer_overshoots_preserve_repair_and_stabilization_fairness() {
    let mut schedule = schedule(0, Duration::from_millis(500), crate::dht::Did::from(0u32));
    let mut stabilization_start_ms = 500;

    for _ in 0..3 {
        assert_eq!(
            schedule
                .poll(stabilization_start_ms, true, finger_status(false))
                .task,
            Some(MaintenanceTask::Stabilize)
        );
        schedule.complete_stabilization(stabilization_start_ms, true);
        let late_wake_ms = schedule.next_stabilize_ms.saturating_add(1);
        assert_eq!(
            schedule.poll(late_wake_ms, true, finger_status(false)).task,
            Some(MaintenanceTask::Repair)
        );
        schedule.complete_repair(late_wake_ms.saturating_add(1), false);
        stabilization_start_ms = schedule.next_stabilize_ms;
    }
}

/// Verifies that repair completion after a stabilization deadline rephases
/// stabilization from actual completion and retains the quiet gap.
#[test]
fn test_repair_overrun_reconciles_stabilization_with_actual_completion() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(15_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(!schedule.complete_stabilization(15_000, false));
    assert_eq!(
        schedule.poll(20_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Repair)
    );

    schedule.complete_repair(31_000, true);

    assert_eq!(
        schedule.poll(31_000, false, finger_status(false)).task,
        None
    );
    assert_eq!(
        schedule.poll(31_050, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
}

/// Verifies that failed repair does not spin immediately and instead waits
/// for the next topology phase before repair becomes eligible again.
#[test]
fn test_failed_repair_waits_for_the_next_topology_phase() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(0u32));

    assert_eq!(
        schedule.poll(15_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(!schedule.complete_stabilization(15_000, false));
    assert_eq!(
        schedule.poll(20_000, false, finger_status(false)).task,
        Some(MaintenanceTask::Repair)
    );
    schedule.complete_repair(20_001, false);

    assert_eq!(schedule.next_wake_ms(20_001, true, false), 30_000);
    assert_eq!(
        schedule.poll(30_000, true, finger_status(false)).task,
        Some(MaintenanceTask::Stabilize)
    );
    assert!(!schedule.complete_stabilization(30_000, true));
    assert_eq!(
        schedule.poll(30_050, true, finger_status(false)).task,
        Some(MaintenanceTask::Repair)
    );
}

#[test]
fn test_late_timer_wake_recomputes_from_absolute_deadline() {
    assert_eq!(remaining_delay(100, 90), Duration::from_millis(10));
    assert_eq!(remaining_delay(100, 125), Duration::ZERO);
}

/// Proves that runnable convergence is jittered and never replayed as backlog.
///
/// The first turn must stay inside the initial jitter window and remain
/// dormant until its exact deadline. Completing that turn late must schedule
/// the next attempt after completion instead of immediately catching up.
#[test]
fn test_finger_convergence_is_jittered_and_never_catches_up_in_a_burst() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(11u32));

    assert_eq!(schedule.poll(0, false, finger_status(true)).task, None);
    let first_deadline = schedule.next_finger_ms;
    assert!((1_000..=11_000).contains(&first_deadline));
    assert_eq!(
        schedule
            .poll(first_deadline - 1, false, finger_status(true))
            .task,
        None
    );
    assert_eq!(
        schedule
            .poll(first_deadline, false, finger_status(true))
            .task,
        Some(MaintenanceTask::ConvergeFingers)
    );

    let late_completion = first_deadline.saturating_add(30_000);
    schedule.complete_finger_convergence(late_completion, finger_status(true));
    assert!(schedule.next_finger_ms > late_completion);
    assert!(schedule.next_finger_ms <= late_completion.saturating_add(2_000));
    assert_ne!(
        schedule
            .poll(late_completion, false, finger_status(true))
            .task,
        Some(MaintenanceTask::ConvergeFingers)
    );
}

/// Proves that only activation from `Dormant` draws the initial jitter
/// window.
///
/// A report applied between polls moves the ring from `AwaitingReport` back
/// to `Runnable`, and periodic revalidation moves a `Converged` table back
/// to `Runnable`; both must use the ordinary zero-failure delay, the same
/// one completing a turn uses, not the 1..=11 second fleet-start window.
#[test]
fn test_returning_to_runnable_uses_the_retry_delay_not_the_initial_window() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(11u32));
    let awaiting = FingerConvergenceStatus::awaiting_report(10_000, 0);
    let _ = schedule.poll(0, false, awaiting);
    assert_eq!(schedule.next_finger_ms, 10_000);

    let _ = schedule.poll(5_000, false, finger_status(true));
    assert!((6_000..=7_000).contains(&schedule.next_finger_ms));

    // Converged, then reopened by revalidation: continuation, not activation.
    let _ = schedule.poll(20_000, false, finger_status(false));
    assert_eq!(schedule.next_finger_ms, u64::MAX);
    let _ = schedule.poll(30_000, false, finger_status(true));
    assert!((31_000..=32_000).contains(&schedule.next_finger_ms));
}

/// Proves that an awaiting-report phase is governed by its lease expiry.
///
/// The scheduler must neither add jitter nor dispatch before the remaining
/// report lease reaches zero; at the exact expiry it may select one finger
/// convergence turn to process timeout state.
#[test]
fn test_in_flight_finger_lookup_wakes_only_at_its_exact_expiry() {
    let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(11u32));
    let awaiting = FingerConvergenceStatus::awaiting_report(10_000, 0);

    assert_eq!(schedule.poll(0, false, awaiting).task, None);
    assert_eq!(schedule.next_finger_ms, 10_000);
    assert_eq!(
        schedule
            .poll(9_999, false, FingerConvergenceStatus::awaiting_report(1, 0))
            .task,
        None
    );
    assert_eq!(
        schedule
            .poll(
                10_000,
                false,
                FingerConvergenceStatus::awaiting_report(0, 0)
            )
            .task,
        Some(MaintenanceTask::ConvergeFingers)
    );
}

/// Proves lifecycle-stable but lifecycle-distinct initial jitter.
///
/// Equal DID and entropy inputs model listener restarts and must replay the
/// same first deadline. Changing only lifecycle entropy must choose another
/// deadline while keeping both results inside the configured initial window.
#[test]
fn test_initial_finger_jitter_uses_replayable_node_lifecycle_entropy() {
    let local = crate::dht::Did::from(4u32);
    let entropy = uuid::Uuid::from_u128(1);
    let mut first = MaintenanceSchedule::new(0, PERIOD, local, entropy);
    let mut listener_restart = MaintenanceSchedule::new(0, PERIOD, local, entropy);
    let mut another_lifecycle =
        MaintenanceSchedule::new(0, PERIOD, local, uuid::Uuid::from_u128(2));
    let _ = first.poll(0, false, finger_status(true));
    let _ = listener_restart.poll(0, false, finger_status(true));
    let _ = another_lifecycle.poll(0, false, finger_status(true));

    assert_eq!(first.next_finger_ms, listener_restart.next_finger_ms);
    assert_ne!(first.next_finger_ms, another_lifecycle.next_finger_ms);
    assert!((1_000..=11_000).contains(&first.next_finger_ms));
    assert!((1_000..=11_000).contains(&another_lifecycle.next_finger_ms));
}

/// Proves repeated browser resumes rephase stale work instead of bursting it.
///
/// Each simulated suspension crosses the stale threshold. Every resumed poll
/// must produce no immediate task and must place the next finger deadline in
/// the fresh initial-jitter window following the resume timestamp.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn test_repeated_browser_resume_rephases_stale_finger_deadlines() {
    let mut schedule = MaintenanceSchedule::new(
        0,
        Duration::from_secs(3_600),
        crate::dht::Did::from(11u32),
        uuid::Uuid::from_u128(1),
    );
    let status = finger_status(true);
    assert_eq!(schedule.poll(0, false, status).task, None);

    for _ in 0..3 {
        let stale_deadline = schedule.next_finger_ms;
        let resumed_at_ms =
            stale_deadline.saturating_add(duration_ms(FINGER_CONVERGENCE_RESUME_REPHASE_THRESHOLD));
        assert_eq!(schedule.poll(resumed_at_ms, false, status).task, None);
        assert!(schedule.next_finger_ms > resumed_at_ms);
        assert!(
            schedule.next_finger_ms
                <= resumed_at_ms
                    .saturating_add(finger_lookup_backoff_ms(0))
                    .saturating_add(duration_ms(FINGER_CONVERGENCE_INITIAL_JITTER))
        );
    }
}
