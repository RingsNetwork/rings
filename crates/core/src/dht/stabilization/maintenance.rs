use std::sync::Arc;
use std::time::Duration;

use web_time::Instant;

use super::storage_repair::StorageRepairOutcome;
use super::Stabilizer;
use super::STABILIZATION_STEP_TIMEOUT;
use super::STABILIZATION_STOP_POLL_INTERVAL;
use crate::lifecycle::StopToken;
use crate::swarm::transport::DATA_CHANNEL_SEND_ACCEPT_BUDGET;
use crate::utils::sleep;

/// The quiet phase reserved for topology stabilization before periodic repair.
const STORAGE_REPAIR_PHASE_OFFSET: Duration = Duration::from_secs(5);
/// The uninterrupted first-frame admission window for one storage repair delivery.
const STORAGE_REPAIR_ADMISSION_BUDGET: Duration = DATA_CHANNEL_SEND_ACCEPT_BUDGET;
/// Separate completed maintenance phases by at least one cooperative poll.
const MAINTENANCE_QUIET_GAP: Duration = STABILIZATION_STOP_POLL_INTERVAL;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaintenanceTask {
    Stabilize,
    Repair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MaintenanceDecision {
    task: Option<MaintenanceTask>,
    periodic_repair_due: bool,
    repair_deferred_for_window: bool,
}

/// Absolute phase schedule for topology and storage maintenance.
///
/// State relation:
/// - `next_stabilize_ms` is advanced only after the selected stabilization run
///   completes, skipping every deadline at or before its completion time.
/// - every elapsed `next_repair_ms` submits a persistent repair intent.
/// - repair may run only after `repair_not_before_ms` and when its first-frame
///   admission budget and a post-repair quiet gap fit before
///   `next_stabilize_ms`.
/// - when repair is pending at stabilization completion, the following
///   stabilization deadline reserves enough time for one repair attempt.
/// - repair is tracked to its final frame. If its tail exceeds the admission
///   estimate, this serial loop cannot overlap it with stabilization and
///   reconciles the next deadline from actual completion.
struct MaintenanceSchedule {
    period_ms: u64,
    next_stabilize_ms: u64,
    next_repair_ms: u64,
    repair_not_before_ms: u64,
    repair_admission_budget_ms: u64,
}

impl MaintenanceSchedule {
    fn new(now_ms: u64, interval: Duration) -> Self {
        let period_ms = duration_ms(interval).max(2);
        let offset_ms = duration_ms(STORAGE_REPAIR_PHASE_OFFSET)
            .min(period_ms / 2)
            .max(1);
        let next_stabilize_ms = now_ms.saturating_add(period_ms);
        Self {
            period_ms,
            next_stabilize_ms,
            next_repair_ms: next_stabilize_ms.saturating_add(offset_ms),
            repair_not_before_ms: now_ms,
            repair_admission_budget_ms: duration_ms(STORAGE_REPAIR_ADMISSION_BUDGET),
        }
    }

    /// Select at most one task. Stabilization wins when both phases are due,
    /// while the repair phase is preserved as an intent rather than dropped.
    fn poll(&mut self, now_ms: u64, repair_pending: bool) -> MaintenanceDecision {
        let periodic_repair_due = self.advance_repair_deadline_if_due(now_ms);
        let effective_repair_pending = repair_pending || periodic_repair_due;
        let stabilization_due = now_ms >= self.next_stabilize_ms;
        let repair_has_window = effective_repair_pending && self.can_start_storage_repair(now_ms);
        let task = if stabilization_due {
            Some(MaintenanceTask::Stabilize)
        } else if repair_has_window {
            Some(MaintenanceTask::Repair)
        } else {
            None
        };

        MaintenanceDecision {
            task,
            periodic_repair_due,
            repair_deferred_for_window: effective_repair_pending
                && !stabilization_due
                && !self.has_storage_repair_window(now_ms),
        }
    }

    /// Reconcile deadlines against completion time. This prevents a long run
    /// from causing immediate catch-up stabilization passes.
    fn complete_stabilization(&mut self, completed_at_ms: u64, repair_pending: bool) -> bool {
        let periodic_repair_due = self.advance_repair_deadline_if_due(completed_at_ms);
        self.next_stabilize_ms =
            next_deadline_after(self.next_stabilize_ms, self.period_ms, completed_at_ms);
        self.repair_not_before_ms =
            completed_at_ms.saturating_add(duration_ms(MAINTENANCE_QUIET_GAP));
        if repair_pending || periodic_repair_due {
            let reserved_deadline = self
                .repair_not_before_ms
                .saturating_add(self.required_repair_window_ms());
            self.next_stabilize_ms = self.next_stabilize_ms.max(reserved_deadline);
        }
        periodic_repair_due
    }

    fn complete_repair(&mut self, completed_at_ms: u64, succeeded: bool) {
        let post_repair_deadline =
            completed_at_ms.saturating_add(duration_ms(MAINTENANCE_QUIET_GAP));
        self.next_stabilize_ms = self.next_stabilize_ms.max(post_repair_deadline);
        self.repair_not_before_ms = if succeeded {
            post_repair_deadline
        } else {
            self.next_repair_ms
        };
    }

    fn advance_repair_deadline_if_due(&mut self, now_ms: u64) -> bool {
        if now_ms < self.next_repair_ms {
            return false;
        }
        self.next_repair_ms = next_deadline_after(self.next_repair_ms, self.period_ms, now_ms);
        true
    }

    fn can_start_storage_repair(&self, now_ms: u64) -> bool {
        now_ms >= self.repair_not_before_ms && self.has_storage_repair_window(now_ms)
    }

    fn has_storage_repair_window(&self, now_ms: u64) -> bool {
        self.storage_repair_window_ms(now_ms) >= self.required_repair_window_ms()
    }

    fn required_repair_window_ms(&self) -> u64 {
        self.repair_admission_budget_ms
            .saturating_add(duration_ms(MAINTENANCE_QUIET_GAP))
    }

    fn storage_repair_window_ms(&self, now_ms: u64) -> u64 {
        self.next_stabilize_ms.saturating_sub(now_ms)
    }

    fn next_wake_ms(&self, now_ms: u64, repair_pending: bool) -> u64 {
        let mut next_ms = self.next_stabilize_ms.min(self.next_repair_ms);
        if !repair_pending {
            return next_ms;
        }
        if self.can_start_storage_repair(now_ms) {
            return now_ms;
        }
        if now_ms < self.repair_not_before_ms
            && self.has_storage_repair_window(self.repair_not_before_ms)
        {
            next_ms = next_ms.min(self.repair_not_before_ms);
        }
        next_ms
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn next_deadline_after(deadline_ms: u64, period_ms: u64, now_ms: u64) -> u64 {
    let elapsed_ms = now_ms.saturating_sub(deadline_ms);
    let periods = elapsed_ms
        .checked_div(period_ms)
        .unwrap_or(0)
        .saturating_add(1);
    let next = deadline_ms.saturating_add(periods.saturating_mul(period_ms));
    if next <= now_ms {
        u64::MAX
    } else {
        next
    }
}

impl Stabilizer {
    /// Run topology stabilization and storage repair in staggered phases.
    pub async fn wait(self: Arc<Self>, interval: Duration) {
        self.wait_with(interval, StopToken::never()).await;
    }

    /// Run staggered maintenance until `stop` asks this loop to exit.
    ///
    /// Repair requests are shared with disconnect handlers and survive missed
    /// phase deadlines. Cooperative stop is observed between phases; the
    /// per-step deadline may still cancel a hung network maintenance future.
    pub async fn wait_with(self: Arc<Self>, interval: Duration, stop: StopToken) {
        let origin = Instant::now();
        let mut schedule = MaintenanceSchedule::new(0, interval);
        loop {
            if stop.should_stop() {
                return;
            }

            let now_ms = monotonic_elapsed_ms(&origin);
            let decision = schedule.poll(now_ms, self.transport.storage_repair_requested());
            if decision.periodic_repair_due {
                self.transport.request_storage_repair();
            }
            if decision.repair_deferred_for_window {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    available_ms = schedule.storage_repair_window_ms(now_ms),
                    required_ms = schedule.required_repair_window_ms(),
                    "STABILIZATION deferred storage repair for an admission window"
                );
            }

            match decision.task {
                Some(MaintenanceTask::Stabilize) => {
                    self.stabilize_topology_with_step_timeout(STABILIZATION_STEP_TIMEOUT)
                        .await;
                    let periodic_repair_due = schedule.complete_stabilization(
                        monotonic_elapsed_ms(&origin),
                        self.transport.storage_repair_requested(),
                    );
                    if periodic_repair_due {
                        self.transport.request_storage_repair();
                    }
                }
                Some(MaintenanceTask::Repair) => {
                    if let Some(outcome) = self.run_requested_storage_repair().await {
                        schedule
                            .complete_repair(monotonic_elapsed_ms(&origin), outcome.is_complete());
                    }
                }
                None => {
                    let deadline_ms =
                        schedule.next_wake_ms(now_ms, self.transport.storage_repair_requested());
                    if !sleep_until_or_stop(&origin, deadline_ms, &stop).await {
                        return;
                    }
                }
            }
        }
    }

    pub(crate) async fn run_requested_storage_repair(&self) -> Option<StorageRepairOutcome> {
        if !self.transport.claim_storage_repair() {
            return None;
        }
        let outcome = self
            .run_step(
                "repair_storage",
                STABILIZATION_STEP_TIMEOUT,
                self.repair_storage(),
            )
            .await
            .unwrap_or(StorageRepairOutcome::Deferred);
        if !outcome.is_complete() {
            self.transport.request_storage_repair();
        }
        Some(outcome)
    }
}

fn monotonic_elapsed_ms(origin: &Instant) -> u64 {
    duration_ms(origin.elapsed())
}

async fn sleep_until_or_stop(origin: &Instant, deadline_ms: u64, stop: &StopToken) -> bool {
    loop {
        if stop.should_stop() {
            return false;
        }
        let delay = remaining_delay(deadline_ms, monotonic_elapsed_ms(origin));
        if delay.is_zero() {
            return !stop.should_stop();
        }
        sleep(delay.min(STABILIZATION_STOP_POLL_INTERVAL)).await;
    }
}

fn remaining_delay(deadline_ms: u64, now_ms: u64) -> Duration {
    Duration::from_millis(deadline_ms.saturating_sub(now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_secs(15);

    #[test]
    fn maintenance_phases_are_staggered_within_each_period() {
        let mut schedule = MaintenanceSchedule::new(0, PERIOD);

        assert_eq!(schedule.poll(14_999, false).task, None);
        assert_eq!(
            schedule.poll(15_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(!schedule.complete_stabilization(15_000, false));
        let repair = schedule.poll(20_000, false);
        assert!(repair.periodic_repair_due);
        assert_eq!(repair.task, Some(MaintenanceTask::Repair));
        schedule.complete_repair(20_000, true);
        assert_eq!(
            schedule.poll(30_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
    }

    #[test]
    fn repeated_stabilization_overruns_preserve_repair_intent() {
        let mut schedule = MaintenanceSchedule::new(0, PERIOD);

        assert_eq!(
            schedule.poll(15_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(schedule.complete_stabilization(21_000, false));
        let missed_phase = schedule.poll(21_000, true);
        assert!(!missed_phase.periodic_repair_due);
        assert_eq!(missed_phase.task, None);
        assert_eq!(
            schedule.poll(21_050, true).task,
            Some(MaintenanceTask::Repair)
        );
    }

    #[test]
    fn long_stabilization_skips_missed_stabilization_deadlines() {
        let mut schedule = MaintenanceSchedule::new(0, PERIOD);

        assert_eq!(
            schedule.poll(15_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(schedule.complete_stabilization(46_000, false));

        assert_eq!(schedule.next_stabilize_ms, 60_000);
        assert_ne!(
            schedule.poll(46_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
    }

    #[test]
    fn stabilization_reserves_a_window_for_pending_repair() {
        let mut schedule = MaintenanceSchedule::new(0, PERIOD);

        assert_eq!(
            schedule.poll(15_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(schedule.complete_stabilization(26_000, false));
        assert_eq!(schedule.next_stabilize_ms, 31_100);
        assert_eq!(schedule.poll(26_000, true).task, None);
        assert_eq!(
            schedule.poll(26_050, true).task,
            Some(MaintenanceTask::Repair)
        );
        schedule.complete_repair(31_050, true);
        assert_eq!(schedule.poll(31_050, false).task, None);
        assert_eq!(
            schedule.poll(31_100, false).task,
            Some(MaintenanceTask::Stabilize)
        );
    }

    #[test]
    fn repair_overrun_reconciles_stabilization_with_actual_completion() {
        let mut schedule = MaintenanceSchedule::new(0, PERIOD);

        assert_eq!(
            schedule.poll(15_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(!schedule.complete_stabilization(15_000, false));
        assert_eq!(
            schedule.poll(20_000, false).task,
            Some(MaintenanceTask::Repair)
        );

        schedule.complete_repair(31_000, true);

        assert_eq!(schedule.poll(31_000, false).task, None);
        assert_eq!(
            schedule.poll(31_050, false).task,
            Some(MaintenanceTask::Stabilize)
        );
    }

    #[test]
    fn failed_repair_waits_for_the_next_topology_phase() {
        let mut schedule = MaintenanceSchedule::new(0, PERIOD);

        assert_eq!(
            schedule.poll(15_000, false).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(!schedule.complete_stabilization(15_000, false));
        assert_eq!(
            schedule.poll(20_000, false).task,
            Some(MaintenanceTask::Repair)
        );
        schedule.complete_repair(20_001, false);

        assert_eq!(schedule.next_wake_ms(20_001, true), 30_000);
        assert_eq!(
            schedule.poll(30_000, true).task,
            Some(MaintenanceTask::Stabilize)
        );
        assert!(!schedule.complete_stabilization(30_000, true));
        assert_eq!(
            schedule.poll(30_050, true).task,
            Some(MaintenanceTask::Repair)
        );
    }

    #[test]
    fn late_timer_wake_recomputes_from_absolute_deadline() {
        assert_eq!(remaining_delay(100, 90), Duration::from_millis(10));
        assert_eq!(remaining_delay(100, 125), Duration::ZERO);
    }
}
