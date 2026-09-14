//! Finite resource model for automatic finger convergence retries.
//!
//! State variables are virtual time, one optional in-flight lookup, the
//! consecutive-failure level, the earliest next issue time, and the emission
//! trace. The environment may deliver success, an invalid report, a duplicate,
//! cancellation, loss followed by timeout, a topology change, or a restart.
//!
//! Safety does not assume eventual delivery. Liveness is conditional: under a
//! fair environment that eventually delivers a valid current report, progress
//! resets the retry level. Under permanent failure, emissions remain rate
//! bounded instead of converging.

use std::collections::BTreeSet;

use crate::dht::finger::finger_lookup_backoff_ms;
use crate::dht::finger::FINGER_LOOKUP_MIN_INTERVAL_MS;
use crate::dht::topology::TopologyEvent;
use crate::dht::FingerFixRequest;

const LOOKUP_TIMEOUT_MS: u64 = 10_000;
const MODEL_DEPTH: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormalModelScope {
    DhtTopology,
    FingerRetry,
}

// Completeness guard: adding a production topology event requires assigning it
// to an executable formal-model scope; there is deliberately no wildcard arm.
fn formal_model_scope(event: &TopologyEvent) -> FormalModelScope {
    match event {
        TopologyEvent::Join { .. }
        | TopologyEvent::Admit { .. }
        | TopologyEvent::Remove { .. }
        | TopologyEvent::UpdateSuccessor { .. }
        | TopologyEvent::Notify { .. }
        | TopologyEvent::Stabilize { .. } => FormalModelScope::DhtTopology,
        TopologyEvent::BeginFingerRevalidation
        | TopologyEvent::AdvanceFingerConvergence { .. }
        | TopologyEvent::ApplyFinger { .. }
        | TopologyEvent::CancelFinger { .. } => FormalModelScope::FingerRetry,
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FingerRetryState {
    now_ms: u64,
    in_flight: bool,
    failure_streak: u8,
    retry_not_before_ms: u64,
    emissions_ms: Vec<u64>,
}

impl FingerRetryState {
    fn initial() -> Self {
        Self {
            now_ms: 0,
            in_flight: false,
            failure_streak: 0,
            retry_not_before_ms: FINGER_LOOKUP_MIN_INTERVAL_MS,
            emissions_ms: Vec::new(),
        }
    }

    fn records_failure(mut self) -> Self {
        self.in_flight = false;
        self.failure_streak = self.failure_streak.saturating_add(1);
        self.retry_not_before_ms = self
            .now_ms
            .saturating_add(finger_lookup_backoff_ms(self.failure_streak));
        self
    }

    fn step(&self, action: FingerRetryAction) -> Self {
        let mut next = self.clone();
        match action {
            FingerRetryAction::AdvanceEarly => {
                next.now_ms = next.retry_not_before_ms.saturating_sub(1).max(next.now_ms);
            }
            FingerRetryAction::AdvanceDue => {
                next.now_ms = next.retry_not_before_ms.max(next.now_ms);
            }
            FingerRetryAction::Issue
                if !next.in_flight && next.now_ms >= next.retry_not_before_ms =>
            {
                next.in_flight = true;
                next.emissions_ms.push(next.now_ms);
            }
            FingerRetryAction::Progress if next.in_flight => {
                next.in_flight = false;
                next.failure_streak = 0;
                next.retry_not_before_ms =
                    next.now_ms.saturating_add(FINGER_LOOKUP_MIN_INTERVAL_MS);
            }
            FingerRetryAction::Invalid | FingerRetryAction::Cancel if next.in_flight => {
                next = next.records_failure();
            }
            FingerRetryAction::Timeout if next.in_flight => {
                next.now_ms = next.now_ms.saturating_add(LOOKUP_TIMEOUT_MS);
                next = next.records_failure();
            }
            FingerRetryAction::TopologyChange if next.in_flight => {
                next.in_flight = false;
                next.failure_streak = next.failure_streak.saturating_add(1);
                next.retry_not_before_ms = next
                    .emissions_ms
                    .last()
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(finger_lookup_backoff_ms(next.failure_streak));
            }
            FingerRetryAction::Restart => {
                next.in_flight = false;
                next.failure_streak = 0;
                next.retry_not_before_ms =
                    next.now_ms.saturating_add(FINGER_LOOKUP_MIN_INTERVAL_MS);
            }
            FingerRetryAction::Issue
            | FingerRetryAction::Progress
            | FingerRetryAction::Invalid
            | FingerRetryAction::Cancel
            | FingerRetryAction::Timeout
            | FingerRetryAction::Lose
            | FingerRetryAction::Duplicate
            | FingerRetryAction::TopologyChange => {}
        }
        next
    }

    fn preserves_emission_interval(&self) -> bool {
        self.emissions_ms.windows(2).all(|window| {
            window
                .get(1)
                .copied()
                .zip(window.first().copied())
                .is_some_and(|(later, earlier)| {
                    later.saturating_sub(earlier) >= FINGER_LOOKUP_MIN_INTERVAL_MS
                })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FingerRetryAction {
    AdvanceEarly,
    AdvanceDue,
    Issue,
    Progress,
    Invalid,
    Cancel,
    Lose,
    Timeout,
    Duplicate,
    TopologyChange,
    Restart,
}

const ACTIONS: [FingerRetryAction; 11] = [
    FingerRetryAction::AdvanceEarly,
    FingerRetryAction::AdvanceDue,
    FingerRetryAction::Issue,
    FingerRetryAction::Progress,
    FingerRetryAction::Invalid,
    FingerRetryAction::Cancel,
    FingerRetryAction::Lose,
    FingerRetryAction::Timeout,
    FingerRetryAction::Duplicate,
    FingerRetryAction::TopologyChange,
    FingerRetryAction::Restart,
];

#[test]
fn test_topology_event_formal_scope_manifest_is_exhaustive() {
    let request = FingerFixRequest::new(0, 1).unwrap_or(FingerFixRequest {
        slot: u16::MAX,
        request_id: u64::MAX,
    });
    let finger_events = [
        TopologyEvent::BeginFingerRevalidation,
        TopologyEvent::AdvanceFingerConvergence { now_ms: 1_000 },
        TopologyEvent::ApplyFinger {
            request,
            successor: crate::dht::Did::from(1u32),
            now_ms: 1_000,
        },
        TopologyEvent::CancelFinger {
            request,
            now_ms: 1_000,
        },
    ];
    assert!(finger_events
        .iter()
        .all(|event| formal_model_scope(event) == FormalModelScope::FingerRetry));
}

#[test]
fn test_finger_retry_model_preserves_rate_and_backoff_under_all_bounded_schedules() {
    let mut seen = BTreeSet::from([FingerRetryState::initial()]);
    let mut frontier = seen.clone();
    for _ in 0..MODEL_DEPTH {
        let mut next_frontier = BTreeSet::new();
        for state in frontier {
            for action in ACTIONS {
                let next = state.step(action);
                assert!(next.preserves_emission_interval());
                if matches!(action, FingerRetryAction::Issue)
                    && (state.in_flight || state.now_ms < state.retry_not_before_ms)
                {
                    assert_eq!(next.emissions_ms, state.emissions_ms);
                }
                if matches!(
                    action,
                    FingerRetryAction::Invalid
                        | FingerRetryAction::Cancel
                        | FingerRetryAction::Timeout
                ) && state.in_flight
                {
                    assert_eq!(
                        next.retry_not_before_ms.saturating_sub(next.now_ms),
                        finger_lookup_backoff_ms(next.failure_streak)
                    );
                }
                if matches!(action, FingerRetryAction::TopologyChange) && state.in_flight {
                    let issued_at_ms = state.emissions_ms.last().copied().unwrap_or(0);
                    assert_eq!(
                        next.retry_not_before_ms,
                        issued_at_ms.saturating_add(finger_lookup_backoff_ms(next.failure_streak))
                    );
                }
                if seen.insert(next.clone()) {
                    next_frontier.insert(next);
                }
            }
        }
        frontier = next_frontier;
    }
}
