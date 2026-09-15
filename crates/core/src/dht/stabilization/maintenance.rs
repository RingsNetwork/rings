//! Timed maintenance scheduler for topology, storage repair, and finger convergence.

use std::sync::Arc;
use std::time::Duration;

use super::storage_repair::StorageRepairOutcome;
use super::Stabilizer;
use super::STABILIZATION_STEP_TIMEOUT;
use super::STABILIZATION_STOP_POLL_INTERVAL;
use crate::dht::finger::finger_lookup_backoff_ms;
use crate::dht::finger::FingerConvergencePhase;
use crate::dht::finger::FingerConvergenceStatus;
use crate::lifecycle::StopToken;
use crate::swarm::transport::DATA_CHANNEL_SEND_ACCEPT_BUDGET;
use crate::utils::try_sleep;
use crate::utils::Instant;

/// The quiet phase reserved for topology stabilization before periodic repair.
const STORAGE_REPAIR_PHASE_OFFSET: Duration = Duration::from_secs(5);
/// The uninterrupted first-frame admission window for one storage repair delivery.
const STORAGE_REPAIR_ADMISSION_BUDGET: Duration = DATA_CHANNEL_SEND_ACCEPT_BUDGET;
/// Separate completed maintenance phases by at least one cooperative poll.
const MAINTENANCE_QUIET_GAP: Duration = STABILIZATION_STOP_POLL_INTERVAL;
/// Fleet-start phase window before the first independently paced finger attempt.
const FINGER_CONVERGENCE_INITIAL_JITTER: Duration = Duration::from_secs(10);
/// An overdue finger deadline beyond this bound is treated as browser suspension,
/// not as runnable backlog, and is spread over a fresh initial phase window.
const FINGER_CONVERGENCE_RESUME_REPHASE_THRESHOLD: Duration = Duration::from_secs(10);
/// A due finger turn may yield to at most stabilization plus its reserved repair.
const MAX_FINGER_PRIORITY_DEFERRALS: u8 = 2;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaintenanceTask {
    /// Run the topology phase: clean peers, notify, start stabilization, and
    /// optionally mark a finger range for later convergence.
    Stabilize,
    /// Run storage repair and inbox delivery for a pending repair intent.
    Repair,
    /// Advance independently paced finger convergence by one lookup/result step.
    ConvergeFingers,
}

#[cfg(all(test, target_family = "wasm"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MaintenancePhaseKind {
    /// Recorded topology phase.
    Stabilize,
    /// Recorded storage repair phase.
    Repair,
    /// Recorded finger convergence phase.
    ConvergeFingers,
}

#[cfg(all(test, target_family = "wasm"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MaintenancePhaseEvent {
    /// Node whose maintenance loop emitted the trace event.
    pub(crate) local: crate::dht::Did,
    /// Maintenance phase that started.
    pub(crate) kind: MaintenancePhaseKind,
    /// Milliseconds since that loop's monotonic origin.
    pub(crate) started_at_ms: u64,
}

#[cfg(all(test, target_family = "wasm"))]
thread_local! {
    /// Per-thread phase trace used only by wasm tests, where separate node loops
    /// share one JavaScript event loop.
    static MAINTENANCE_PHASE_TRACE: std::cell::RefCell<Vec<MaintenancePhaseEvent>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(all(test, target_family = "wasm"))]
/// Clear the wasm-only maintenance phase trace before a test scenario.
pub(crate) fn reset_maintenance_phase_trace_for_test() {
    MAINTENANCE_PHASE_TRACE.with(|trace| trace.borrow_mut().clear());
}

#[cfg(all(test, target_family = "wasm"))]
/// Return phase trace entries for one local DID.
pub(crate) fn maintenance_phase_trace_for_test(
    local: crate::dht::Did,
) -> Vec<MaintenancePhaseEvent> {
    MAINTENANCE_PHASE_TRACE.with(|trace| {
        trace
            .borrow()
            .iter()
            .copied()
            .filter(|event| event.local == local)
            .collect()
    })
}

#[cfg(all(test, target_family = "wasm"))]
/// Append one wasm-only maintenance phase trace event.
fn record_maintenance_phase_for_test(
    local: crate::dht::Did,
    task: MaintenanceTask,
    started_at_ms: u64,
) {
    let kind = match task {
        MaintenanceTask::Stabilize => MaintenancePhaseKind::Stabilize,
        MaintenanceTask::Repair => MaintenancePhaseKind::Repair,
        MaintenanceTask::ConvergeFingers => MaintenancePhaseKind::ConvergeFingers,
    };
    MAINTENANCE_PHASE_TRACE.with(|trace| {
        trace.borrow_mut().push(MaintenancePhaseEvent {
            local,
            kind,
            started_at_ms,
        });
    });
}

#[cfg(not(all(test, target_family = "wasm")))]
/// Non-wasm builds do not record phase traces.
fn record_maintenance_phase_for_test(
    _local: crate::dht::Did,
    _task: MaintenanceTask,
    _started_at_ms: u64,
) {
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MaintenanceDecision {
    /// The single task selected for this poll, if any.
    task: Option<MaintenanceTask>,
    /// Whether an elapsed periodic repair deadline created a repair intent.
    periodic_repair_due: bool,
    /// Whether repair is pending but cannot fit before the next topology phase.
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
///   repair turn takes precedence over the following stabilization, even when
///   timer jitter wakes the loop after the reserved deadline.
/// - every repair attempt consumes that precedence before stabilization may
///   reserve another turn, preserving fairness between both maintenance tasks.
/// - repair is tracked to its final frame. If its tail exceeds the admission
///   estimate, this serial loop cannot overlap it with stabilization and
///   reconciles the next deadline from actual completion.
/// - finger retries use the convergence state's consecutive-failure level.
///   Their node-lifecycle-randomized full-jitter window grows with exponential
///   backoff; only applied finger evidence resets that level. A listener restart
///   reuses its node phase, while a long browser suspension rephases stale work
///   instead of immediately submitting it. A due turn may yield to at most two
///   higher-priority phases before it is reserved, so topology and storage work
///   cannot starve convergence indefinitely.
struct MaintenanceSchedule {
    /// Shared period for stabilization and periodic storage-repair deadlines.
    period_ms: u64,
    /// Greatest monotonic loop timestamp observed by polling or completion.
    ///
    /// Observation gaps are measured from this value to distinguish a suspended
    /// host from ordinary timer delay. Updates use `max` so a stale callback
    /// cannot move scheduler time backward.
    last_observed_ms: u64,
    /// Absolute timestamp for the next topology phase.
    next_stabilize_ms: u64,
    /// Absolute timestamp at which periodic storage repair becomes due.
    next_repair_ms: u64,
    /// Earliest timestamp at which a pending repair may start.
    repair_not_before_ms: u64,
    /// Admission window the storage phase reserves before the next topology phase.
    repair_admission_budget_ms: u64,
    /// Whether stabilization reserved the next available turn for pending repair.
    repair_turn_reserved: bool,
    /// Absolute monotonic timestamp for the next independently paced finger turn.
    ///
    /// `u64::MAX` represents an inactive convergence machine. Awaiting phases
    /// replace it with their lease expiry; runnable phases replace it with a
    /// jittered retry deadline.
    next_finger_ms: u64,
    /// Finger convergence phase observed during the previous reconciliation.
    ///
    /// A phase transition changes which clock owns the next wake and therefore
    /// forces `next_finger_ms` to be recomputed exactly once.
    finger_phase_last_poll: FingerConvergencePhase,
    /// Consecutive lookup failures observed during the previous reconciliation.
    ///
    /// A changed streak re-arms a runnable attempt with the corresponding
    /// exponential backoff floor; successful applied evidence resets it upstream.
    finger_failure_streak: u8,
    /// Replayable pseudo-random state used to spread this node's finger attempts.
    ///
    /// It is seeded from the local DID and node-lifecycle entropy, then advanced
    /// exactly once for each newly selected initial or retry delay.
    finger_jitter_state: u64,
    /// Number of consecutive due finger turns yielded to topology or repair.
    ///
    /// The count resets when finger work runs or ceases to be ready. Reaching
    /// `MAX_FINGER_PRIORITY_DEFERRALS` reserves the next decision for convergence.
    finger_priority_deferrals: u8,
}

impl MaintenanceSchedule {
    /// Create a schedule whose first topology phase starts after `interval` and
    /// whose first storage phase is offset from topology by a bounded window.
    fn new(
        now_ms: u64,
        interval: Duration,
        local: crate::dht::Did,
        jitter_entropy: uuid::Uuid,
    ) -> Self {
        let period_ms = duration_ms(interval).max(2);
        let offset_ms = duration_ms(STORAGE_REPAIR_PHASE_OFFSET)
            .min(period_ms / 2)
            .max(1);
        let next_stabilize_ms = now_ms.saturating_add(period_ms);
        Self {
            period_ms,
            last_observed_ms: now_ms,
            next_stabilize_ms,
            next_repair_ms: next_stabilize_ms.saturating_add(offset_ms),
            repair_not_before_ms: now_ms,
            repair_admission_budget_ms: duration_ms(STORAGE_REPAIR_ADMISSION_BUDGET),
            repair_turn_reserved: false,
            next_finger_ms: u64::MAX,
            finger_phase_last_poll: FingerConvergencePhase::Inactive,
            finger_failure_streak: 0,
            finger_jitter_state: finger_jitter_seed(local, jitter_entropy),
            finger_priority_deferrals: 0,
        }
    }

    /// Select at most one task. Stabilization wins when both phases are due,
    /// while the repair phase is preserved as an intent rather than dropped.
    fn poll(
        &mut self,
        now_ms: u64,
        repair_pending: bool,
        finger_status: FingerConvergenceStatus,
    ) -> MaintenanceDecision {
        // A large observation gap usually means the browser or host suspended
        // the maintenance loop rather than that every missed finger slot is due.
        let observation_gap_ms = now_ms.saturating_sub(self.last_observed_ms);
        self.last_observed_ms = self.last_observed_ms.max(now_ms);
        self.reconcile_finger_status(now_ms, finger_status);
        self.rephase_stale_finger_deadline(now_ms, observation_gap_ms, finger_status);
        // Convert elapsed periodic repair deadlines into sticky transport-level
        // intents so an overrunning topology phase does not drop storage repair.
        let periodic_repair_due = self.advance_repair_deadline_if_due(now_ms);
        let effective_repair_pending = repair_pending || periodic_repair_due;
        if !effective_repair_pending {
            self.repair_turn_reserved = false;
        }
        // A reserved repair turn can start as soon as its quiet gap has elapsed,
        // even if timer jitter woke the loop after the originally reserved point.
        let reserved_repair_ready = self.reserved_repair_ready(now_ms, effective_repair_pending);
        let stabilization_due = now_ms >= self.next_stabilize_ms;
        let repair_has_window = effective_repair_pending && self.can_start_storage_repair(now_ms);
        let finger_ready = finger_status.may_advance() && now_ms >= self.next_finger_ms;
        // After a bounded number of yields, finger convergence gets a turn even
        // when topology or storage are also due.
        let finger_turn_reserved =
            finger_ready && self.finger_priority_deferrals >= MAX_FINGER_PRIORITY_DEFERRALS;
        let task = if finger_turn_reserved {
            Some(MaintenanceTask::ConvergeFingers)
        } else if reserved_repair_ready {
            Some(MaintenanceTask::Repair)
        } else if stabilization_due {
            Some(MaintenanceTask::Stabilize)
        } else if repair_has_window {
            Some(MaintenanceTask::Repair)
        } else if finger_ready {
            Some(MaintenanceTask::ConvergeFingers)
        } else {
            None
        };

        self.finger_priority_deferrals = match (finger_ready, task) {
            (_, Some(MaintenanceTask::ConvergeFingers)) | (false, _) => 0,
            (true, Some(_)) => self.finger_priority_deferrals.saturating_add(1),
            (true, None) => self.finger_priority_deferrals,
        };

        MaintenanceDecision {
            task,
            periodic_repair_due,
            repair_deferred_for_window: effective_repair_pending
                && !self.repair_turn_reserved
                && !stabilization_due
                && !self.has_storage_repair_window(now_ms),
        }
    }

    /// Reconcile deadlines against completion time. This prevents a long run
    /// from causing immediate catch-up stabilization passes.
    fn complete_stabilization(&mut self, completed_at_ms: u64, repair_pending: bool) -> bool {
        self.last_observed_ms = self.last_observed_ms.max(completed_at_ms);
        let periodic_repair_due = self.advance_repair_deadline_if_due(completed_at_ms);
        self.next_stabilize_ms =
            next_deadline_after(self.next_stabilize_ms, self.period_ms, completed_at_ms);
        self.repair_not_before_ms =
            completed_at_ms.saturating_add(duration_ms(MAINTENANCE_QUIET_GAP));
        self.repair_turn_reserved = repair_pending || periodic_repair_due;
        if self.repair_turn_reserved {
            let reserved_deadline = self
                .repair_not_before_ms
                .saturating_add(self.required_repair_window_ms());
            self.next_stabilize_ms = self.next_stabilize_ms.max(reserved_deadline);
        }
        periodic_repair_due
    }

    /// Reconcile deadlines after one storage phase, preserving a retry intent
    /// only when the repair did not finish.
    fn complete_repair(&mut self, completed_at_ms: u64, succeeded: bool) {
        self.last_observed_ms = self.last_observed_ms.max(completed_at_ms);
        self.repair_turn_reserved = false;
        let post_repair_deadline =
            completed_at_ms.saturating_add(duration_ms(MAINTENANCE_QUIET_GAP));
        self.next_stabilize_ms = self.next_stabilize_ms.max(post_repair_deadline);
        self.repair_not_before_ms = if succeeded {
            post_repair_deadline
        } else {
            self.next_repair_ms
        };
    }

    /// Reconcile the finger deadline from the state observed after one turn.
    ///
    /// Awaiting phases preserve their exact remaining lease, inactive work is
    /// removed from the wake set, and runnable work receives a fresh jittered
    /// retry measured from actual completion. Measuring from completion prevents
    /// a slow attempt from creating an immediate catch-up burst.
    fn complete_finger_convergence(
        &mut self,
        completed_at_ms: u64,
        status: FingerConvergenceStatus,
    ) {
        self.last_observed_ms = self.last_observed_ms.max(completed_at_ms);
        self.finger_phase_last_poll = status.phase();
        self.finger_failure_streak = status.failure_streak();
        self.next_finger_ms = match status.phase() {
            FingerConvergencePhase::Inactive => u64::MAX,
            FingerConvergencePhase::AwaitingReport { remaining_ms } => {
                completed_at_ms.saturating_add(remaining_ms)
            }
            FingerConvergencePhase::AwaitingAdmission { remaining_ms } => {
                completed_at_ms.saturating_add(remaining_ms)
            }
            FingerConvergencePhase::Runnable => {
                completed_at_ms.saturating_add(self.next_finger_delay_ms(status.failure_streak()))
            }
        };
    }

    /// Align the scheduler-owned wake time with the peer-ring convergence phase.
    ///
    /// Awaiting phases copy their remaining lease into an absolute deadline.
    /// Becoming runnable from `Inactive` with no failure history (the node's
    /// first successor was admitted, the fleet-start case) chooses the initial
    /// jitter window; every other entry into `Runnable`, and a changed failure
    /// streak, chooses the ordinary retry delay for the current streak, the
    /// same delay [`Self::complete_finger_convergence`] uses. An unchanged
    /// runnable phase keeps its existing deadline so ordinary polling cannot
    /// continuously postpone work.
    fn reconcile_finger_status(&mut self, now_ms: u64, status: FingerConvergenceStatus) {
        // A pace change means the retry floor changed; a phase change means a
        // different scheduler clock now owns the next finger wake.
        let pace_changed = status.failure_streak() != self.finger_failure_streak;
        let phase_changed = status.phase() != self.finger_phase_last_poll;
        let activated = matches!(
            self.finger_phase_last_poll,
            FingerConvergencePhase::Inactive
        );
        match status.phase() {
            FingerConvergencePhase::Inactive => self.next_finger_ms = u64::MAX,
            FingerConvergencePhase::AwaitingReport { remaining_ms } => {
                self.next_finger_ms = now_ms.saturating_add(remaining_ms);
            }
            FingerConvergencePhase::AwaitingAdmission { remaining_ms } => {
                self.next_finger_ms = now_ms.saturating_add(remaining_ms);
            }
            FingerConvergencePhase::Runnable if phase_changed => {
                let delay_ms = if activated && status.failure_streak() == 0 {
                    self.next_initial_finger_delay_ms()
                } else {
                    self.next_finger_delay_ms(status.failure_streak())
                };
                self.next_finger_ms = now_ms.saturating_add(delay_ms);
            }
            FingerConvergencePhase::Runnable if pace_changed => {
                self.next_finger_ms =
                    now_ms.saturating_add(self.next_finger_delay_ms(status.failure_streak()));
            }
            FingerConvergencePhase::Runnable => {}
        }
        self.finger_phase_last_poll = status.phase();
        self.finger_failure_streak = status.failure_streak();
    }

    /// Rephase overdue runnable work after a suspension-sized observation gap.
    ///
    /// Rephasing requires both the polling gap and deadline lateness to cross the
    /// resume threshold. This avoids treating normal timer jitter as suspension,
    /// and clears priority deferrals because the newly phased turn has not yet
    /// yielded to another task.
    fn rephase_stale_finger_deadline(
        &mut self,
        now_ms: u64,
        observation_gap_ms: u64,
        status: FingerConvergenceStatus,
    ) {
        // `stale_by_ms` ignores future deadlines via saturating subtraction.
        let stale_by_ms = now_ms.saturating_sub(self.next_finger_ms);
        if matches!(status.phase(), FingerConvergencePhase::Runnable)
            && self.next_finger_ms != u64::MAX
            && observation_gap_ms >= duration_ms(FINGER_CONVERGENCE_RESUME_REPHASE_THRESHOLD)
            && stale_by_ms >= duration_ms(FINGER_CONVERGENCE_RESUME_REPHASE_THRESHOLD)
        {
            self.next_finger_ms = now_ms.saturating_add(self.next_initial_finger_delay_ms());
            self.finger_priority_deferrals = 0;
        }
    }

    /// Draw a full-jitter retry delay for a failed or continuing lookup.
    ///
    /// The failure streak selects the exponential backoff floor. The mixed state
    /// contributes an inclusive `[0, floor]` offset, so the returned delay lies
    /// in `[floor, 2 * floor]` with saturating arithmetic at the numeric limit.
    fn next_finger_delay_ms(&mut self, failure_streak: u8) -> u64 {
        self.finger_jitter_state = mix_jitter(self.finger_jitter_state);
        let retry_floor_ms = finger_lookup_backoff_ms(failure_streak);
        retry_floor_ms.saturating_add(self.finger_jitter_state % retry_floor_ms.saturating_add(1))
    }

    /// Draw the first delay after convergence enters a runnable phase.
    ///
    /// The minimum delay is the zero-failure lookup backoff. A node-specific
    /// inclusive jitter in the configured initial window spreads simultaneous
    /// starts while retaining deterministic replay for one node lifecycle.
    fn next_initial_finger_delay_ms(&mut self) -> u64 {
        self.finger_jitter_state = mix_jitter(self.finger_jitter_state);
        let initial_jitter_ms = duration_ms(FINGER_CONVERGENCE_INITIAL_JITTER);
        finger_lookup_backoff_ms(0)
            .saturating_add(self.finger_jitter_state % initial_jitter_ms.saturating_add(1))
    }

    /// Advance the periodic repair deadline if this poll crossed it.
    fn advance_repair_deadline_if_due(&mut self, now_ms: u64) -> bool {
        if now_ms < self.next_repair_ms {
            return false;
        }
        self.next_repair_ms = next_deadline_after(self.next_repair_ms, self.period_ms, now_ms);
        true
    }

    /// Whether repair can start now and still keep the required quiet gap.
    fn can_start_storage_repair(&self, now_ms: u64) -> bool {
        now_ms >= self.repair_not_before_ms && self.has_storage_repair_window(now_ms)
    }

    /// Whether a repair turn reserved by stabilization is ready to run.
    fn reserved_repair_ready(&self, now_ms: u64, repair_pending: bool) -> bool {
        self.repair_turn_reserved && repair_pending && now_ms >= self.repair_not_before_ms
    }

    /// Whether the time before the next topology phase can fit storage repair.
    fn has_storage_repair_window(&self, now_ms: u64) -> bool {
        self.storage_repair_window_ms(now_ms) >= self.required_repair_window_ms()
    }

    /// Minimum time reserved for storage admission plus a cooperative quiet gap.
    fn required_repair_window_ms(&self) -> u64 {
        self.repair_admission_budget_ms
            .saturating_add(duration_ms(MAINTENANCE_QUIET_GAP))
    }

    /// Available time before the next topology phase begins.
    fn storage_repair_window_ms(&self, now_ms: u64) -> u64 {
        self.next_stabilize_ms.saturating_sub(now_ms)
    }

    /// Next absolute timestamp the maintenance loop should wake for.
    fn next_wake_ms(&self, now_ms: u64, repair_pending: bool, finger_pending: bool) -> u64 {
        let mut next_ms = self.next_stabilize_ms.min(self.next_repair_ms);
        if finger_pending {
            next_ms = next_ms.min(self.next_finger_ms);
        }
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

/// Derive the initial replayable jitter state from node identity and lifecycle.
///
/// The FNV-style fold makes listener restarts within one node lifecycle reuse
/// the same phase, while a new lifecycle UUID produces a different schedule.
/// Wrapping multiplication is intentional because the state is entropy for
/// pacing, not a cryptographic digest.
fn finger_jitter_seed(local: crate::dht::Did, entropy: uuid::Uuid) -> u64 {
    local
        .as_bytes()
        .iter()
        .chain(entropy.as_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |seed, byte| {
            seed.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(*byte)
        })
}

/// Advance the small xorshift state used for finger scheduling jitter.
///
/// The transform is deterministic and cheap, which is sufficient for dispersing
/// maintenance deadlines. It must not be used for cryptographic randomness or
/// any decision whose unpredictability is a security property.
fn mix_jitter(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
/// Return the first runnable finger deadline for a deterministic test node.
///
/// The helper exposes scheduler timing without running the async loop. Callers
/// provide identity, lifecycle entropy, and failure streak so tests can assert
/// the exact initial/retry window selected by production reconciliation.
pub(crate) fn finger_schedule_deadline_for_test(
    local: crate::dht::Did,
    jitter_entropy: uuid::Uuid,
    failure_streak: u8,
) -> u64 {
    let mut schedule = MaintenanceSchedule::new(0, Duration::from_secs(15), local, jitter_entropy);
    let _ = schedule.poll(0, false, FingerConvergenceStatus::new(true, failure_streak));
    schedule.next_finger_ms
}

#[cfg(all(test, not(target_family = "wasm")))]
/// Return the absolute wake deadline for a lookup already awaiting its report.
///
/// `listener_now_ms` supplies the scheduler's monotonic origin and `status`
/// carries the remaining report lease. The result verifies that awaiting work
/// bypasses jitter and wakes exactly at lease expiry.
pub(crate) fn finger_awaiting_report_deadline_for_test(
    listener_now_ms: u64,
    status: FingerConvergenceStatus,
) -> u64 {
    let mut schedule = MaintenanceSchedule::new(
        listener_now_ms,
        Duration::from_secs(15),
        crate::dht::Did::from(1u32),
        uuid::Uuid::from_u128(1),
    );
    let _ = schedule.poll(listener_now_ms, false, status);
    schedule.next_finger_ms
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
/// Simulate a suspension-sized polling gap and return resume timing.
///
/// The pair contains the synthetic resume timestamp followed by the newly
/// rephased finger deadline. The helper also checks internally that resumption
/// does not immediately dispatch the stale convergence turn.
pub(crate) fn finger_schedule_resumed_deadline_for_test(
    local: crate::dht::Did,
    jitter_entropy: uuid::Uuid,
) -> (u64, u64) {
    let mut schedule = MaintenanceSchedule::new(0, Duration::from_secs(15), local, jitter_entropy);
    let status = FingerConvergenceStatus::new(true, 0);
    let _ = schedule.poll(0, false, status);
    let resumed_at_ms = schedule
        .next_finger_ms
        .saturating_add(duration_ms(FINGER_CONVERGENCE_RESUME_REPHASE_THRESHOLD));
    let decision = schedule.poll(resumed_at_ms, false, status);
    debug_assert_ne!(decision.task, Some(MaintenanceTask::ConvergeFingers));
    (resumed_at_ms, schedule.next_finger_ms)
}

/// Convert a duration to milliseconds, saturating at `u64::MAX`.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// First periodic deadline strictly after `now_ms`.
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
    /// Repair requests are shared with disconnect handlers and successor-head
    /// changes and survive missed phase deadlines. Cooperative stop is observed between phases; the
    /// per-step deadline may still cancel a hung network maintenance future.
    pub async fn wait_with(self: Arc<Self>, interval: Duration, stop: StopToken) {
        let origin = Instant::now();
        let mut schedule =
            MaintenanceSchedule::new(0, interval, self.dht.did, self.dht.finger_jitter_entropy());
        loop {
            if stop.should_stop() {
                return;
            }

            let now_ms = monotonic_elapsed_ms(&origin);
            // Finger status is advisory for scheduling; failure to inspect it
            // should not stop topology and storage maintenance.
            let finger_status = match self.dht.finger_convergence_status() {
                Ok(status) => status,
                Err(error) => {
                    tracing::error!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        error = ?error,
                        "STABILIZATION failed to inspect finger convergence"
                    );
                    FingerConvergenceStatus::inactive()
                }
            };
            let decision = schedule.poll(
                now_ms,
                self.transport.storage_repair_requested(),
                finger_status,
            );
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

            if let Some(task) = decision.task {
                self.run_maintenance_task(task, now_ms, finger_status, &origin, &mut schedule)
                    .await;
            } else {
                // Recompute the sleep deadline after recording any repair intent,
                // so a newly pending repair can wake before the next period.
                let deadline_ms = schedule.next_wake_ms(
                    now_ms,
                    self.transport.storage_repair_requested(),
                    finger_status.may_advance(),
                );
                if !sleep_until_or_stop(&origin, deadline_ms, &stop).await {
                    return;
                }
            }
        }
    }

    /// Execute one selected maintenance task and reconcile scheduler state.
    ///
    /// Topology completion may publish a sticky repair intent, repair completion
    /// records whether another pass is required, and finger completion re-reads
    /// the peer-ring phase before choosing its next delay. Every branch uses the
    /// actual monotonic completion time, so overruns never create catch-up bursts.
    async fn run_maintenance_task(
        &self,
        task: MaintenanceTask,
        now_ms: u64,
        finger_status: FingerConvergenceStatus,
        origin: &Instant,
        schedule: &mut MaintenanceSchedule,
    ) {
        record_maintenance_phase_for_test(self.dht.did, task, now_ms);
        match task {
            MaintenanceTask::Stabilize => {
                self.stabilize_scheduled_topology_with_step_timeout(STABILIZATION_STEP_TIMEOUT)
                    .await;
                let periodic_repair_due = schedule.complete_stabilization(
                    monotonic_elapsed_ms(origin),
                    self.transport.storage_repair_requested(),
                );
                if periodic_repair_due {
                    self.transport.request_storage_repair();
                }
            }
            MaintenanceTask::Repair => {
                if let Some(outcome) = self.run_requested_storage_maintenance().await {
                    schedule.complete_repair(monotonic_elapsed_ms(origin), outcome.is_complete());
                }
            }
            MaintenanceTask::ConvergeFingers => {
                let step_completed = self
                    .run_step(
                        "converge_fingers",
                        STABILIZATION_STEP_TIMEOUT,
                        self.advance_finger_convergence(),
                    )
                    .await
                    .is_some();
                let completed_status = self
                    .dht
                    .finger_convergence_status()
                    .unwrap_or(finger_status);
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    step_completed,
                    failure_streak = completed_status.failure_streak(),
                    "STABILIZATION finger convergence paced after outcome"
                );
                schedule
                    .complete_finger_convergence(monotonic_elapsed_ms(origin), completed_status);
            }
        }
    }

    /// Run the storage maintenance phase if one was requested: deliver this node's own inbox,
    /// then restore placement (ownership hand-off and additive republish).
    pub(crate) async fn run_requested_storage_maintenance(&self) -> Option<StorageRepairOutcome> {
        if !self.transport.claim_storage_repair() {
            return None;
        }
        Some(
            self.maintain_storage_with_step_timeout(STABILIZATION_STEP_TIMEOUT)
                .await,
        )
    }

    /// The storage maintenance phase as two steps; an incomplete repair re-requests the phase.
    pub(super) async fn maintain_storage_with_step_timeout(
        &self,
        timeout: Duration,
    ) -> StorageRepairOutcome {
        // The intent to deliver this node's own relay inbox; the swarm interprets it.
        self.run_step("deliver_inbox", timeout, self.inbox.deliver_inbox())
            .await;
        let outcome = self
            .run_step("repair_storage", timeout, self.repair_storage())
            .await
            .unwrap_or(StorageRepairOutcome::Deferred);
        if !outcome.is_complete() {
            self.transport.request_storage_repair();
        }
        outcome
    }
}

/// Milliseconds since the loop origin according to the monotonic clock.
fn monotonic_elapsed_ms(origin: &Instant) -> u64 {
    duration_ms(origin.elapsed())
}

/// Sleep cooperatively until an absolute loop deadline or a stop request.
async fn sleep_until_or_stop(origin: &Instant, deadline_ms: u64, stop: &StopToken) -> bool {
    loop {
        if stop.should_stop() {
            return false;
        }
        let delay = remaining_delay(deadline_ms, monotonic_elapsed_ms(origin));
        if delay.is_zero() {
            return !stop.should_stop();
        }
        if !try_sleep(delay.min(STABILIZATION_STOP_POLL_INTERVAL)).await {
            tracing::error!("stopping stabilization maintenance after timer scheduling failed");
            return false;
        }
    }
}

/// Remaining delay before an absolute loop deadline.
fn remaining_delay(deadline_ms: u64, now_ms: u64) -> Duration {
    Duration::from_millis(deadline_ms.saturating_sub(now_ms))
}

#[cfg(test)]
mod tests {
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
        FingerConvergenceStatus::new(pending, 0)
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

    /// Proves that only activation from `Inactive` draws the initial jitter
    /// window.
    ///
    /// A report applied between polls moves the ring from `AwaitingReport` back
    /// to `Runnable`; the scheduler must then use the ordinary zero-failure
    /// delay, the same one completing a turn uses, not the 1..=11 second
    /// fleet-start window.
    #[test]
    fn test_returning_to_runnable_uses_the_retry_delay_not_the_initial_window() {
        let mut schedule = schedule(0, PERIOD, crate::dht::Did::from(11u32));
        let awaiting = FingerConvergenceStatus::awaiting_report(10_000, 0);
        let _ = schedule.poll(0, false, awaiting);
        assert_eq!(schedule.next_finger_ms, 10_000);

        let _ = schedule.poll(5_000, false, finger_status(true));

        assert!((6_000..=7_000).contains(&schedule.next_finger_ms));
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
            let resumed_at_ms = stale_deadline
                .saturating_add(duration_ms(FINGER_CONVERGENCE_RESUME_REPHASE_THRESHOLD));
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
}

/// Additional schedule tests that require crate-level test fixtures.
#[cfg(test)]
mod schedule_tests;
