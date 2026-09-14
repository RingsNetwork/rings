//! Finite model checking over the production finger-convergence transition.
//!
//! State variables are the real [`TopologyState`], process-monotonic time,
//! emitted request identities, delayed reports, and the next identity supplied
//! by the effect boundary. The environment may advance before or to a
//! deadline, deliver progress, invalid evidence, or a report exactly at its
//! expiry, cancel, lose, duplicate, change topology, or restart.
//!
//! Safety does not assume eventual delivery. Liveness is conditional on a fair
//! scheduler and eventual valid delivery. The checker executes
//! [`crate::dht::topology::step`] itself, so production and model semantics
//! cannot drift behind a scope manifest.

use std::collections::BTreeSet;

use crate::dht::finger::finger_lookup_backoff_ms;
use crate::dht::finger::FINGER_LOOKUP_MIN_INTERVAL_MS;
use crate::dht::topology::step;
use crate::dht::topology::ConditionalFingerUpdate;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyAction;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
use crate::dht::Did;
use crate::dht::FingerFixRequest;

const MODEL_DEPTH: usize = 9;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormalModelScope {
    DhtTopology,
    FingerRetry,
}

const DHT_ONLY: &[FormalModelScope] = &[FormalModelScope::DhtTopology];
const FINGER_ONLY: &[FormalModelScope] = &[FormalModelScope::FingerRetry];
const DHT_AND_FINGER: &[FormalModelScope] =
    &[FormalModelScope::DhtTopology, FormalModelScope::FingerRetry];

/// Completeness manifest for model composition. Cross-domain topology events
/// belong to both models because they invalidate finger evidence in production.
fn formal_model_scopes(event: &TopologyEvent) -> &'static [FormalModelScope] {
    match event {
        TopologyEvent::Join { .. }
        | TopologyEvent::Admit { .. }
        | TopologyEvent::Remove { .. }
        | TopologyEvent::UpdateSuccessor { .. }
        | TopologyEvent::Stabilize { .. } => DHT_AND_FINGER,
        TopologyEvent::Notify { .. } => DHT_ONLY,
        TopologyEvent::BeginFingerRevalidation
        | TopologyEvent::AdvanceFingerConvergence { .. }
        | TopologyEvent::ApplyFinger { .. }
        | TopologyEvent::CancelFinger { .. } => FINGER_ONLY,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FingerRetryState {
    topology: TopologyState,
    now_ms: u64,
    emissions: Vec<(uuid::Uuid, u64, u64)>,
    delayed_reports: Vec<FingerFixRequest>,
    next_request_id: u128,
    next_topology_mutation: u8,
    run_generation: u64,
}

impl FingerRetryState {
    fn initial() -> Self {
        let local = Did::from(0u32);
        let joined = step(
            &TopologyState::new(local, Vec::new(), None, vec![None; 4], 0),
            TopologyEvent::Join {
                peer: Did::from(8u32),
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        Self {
            topology: joined.state,
            now_ms: 0,
            emissions: Vec::new(),
            delayed_reports: Vec::new(),
            next_request_id: 1,
            next_topology_mutation: 0,
            run_generation: 0,
        }
    }

    fn current_request(&self) -> Option<FingerFixRequest> {
        self.topology.finger_convergence_projection().in_flight
    }

    fn next_deadline_ms(&self) -> u64 {
        let projection = self.topology.finger_convergence_projection();
        if let Some(expires_at_ms) = projection.expires_at_ms {
            return expires_at_ms.max(self.now_ms);
        }
        let interval_deadline = projection
            .last_issued_at_ms
            .map(|issued| issued.saturating_add(FINGER_LOOKUP_MIN_INTERVAL_MS))
            .unwrap_or(self.now_ms);
        projection
            .retry_not_before_ms
            .unwrap_or(self.now_ms)
            .max(interval_deadline)
            .max(self.now_ms)
    }

    fn advance_at(&self, now_ms: u64) -> Self {
        let request_id = uuid::Uuid::from_u128(self.next_request_id);
        let output = step(
            &self.topology,
            TopologyEvent::AdvanceFingerConvergence { now_ms, request_id },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        next.now_ms = now_ms;
        let mut emitted = false;
        for action in output.actions {
            if let TopologyAction::FindSuccessorForFix { request, .. } = action {
                next.emissions
                    .push((request.request_id(), now_ms, self.run_generation));
                emitted = true;
            }
        }
        if emitted {
            next.next_request_id = next.next_request_id.saturating_add(1);
        }
        next
    }

    fn apply_current(&self, successor: Did) -> Self {
        self.apply_current_at(successor, self.now_ms)
    }

    fn apply_current_at(&self, successor: Did, now_ms: u64) -> Self {
        let Some(request) = self.current_request() else {
            return self.clone();
        };
        let output = step(
            &self.topology,
            TopologyEvent::ApplyFinger {
                request,
                successor,
                now_ms,
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        next.now_ms = now_ms;
        next.delayed_reports.push(request);
        next
    }

    fn transition(&self, action: FingerRetryAction) -> Self {
        match action {
            FingerRetryAction::AdvanceBeforeDeadline => {
                let deadline = self.next_deadline_ms();
                if deadline > self.now_ms {
                    self.advance_at(deadline.saturating_sub(1))
                } else {
                    self.clone()
                }
            }
            FingerRetryAction::AdvanceToDeadline => self.advance_at(self.next_deadline_ms()),
            FingerRetryAction::Progress => {
                let Some(request) = self.current_request() else {
                    return self.clone();
                };
                self.apply_current(self.topology.local + Did::power_of_two(request.slot_index()))
            }
            FingerRetryAction::Invalid => {
                let Some(request) = self.current_request() else {
                    return self.clone();
                };
                let Some(previous_slot) = request.slot_index().checked_sub(1) else {
                    return self.clone();
                };
                self.apply_current(self.topology.local + Did::power_of_two(previous_slot))
            }
            FingerRetryAction::LateReport => {
                let projection = self.topology.finger_convergence_projection();
                let (Some(request), Some(expires_at_ms)) =
                    (projection.in_flight, projection.expires_at_ms)
                else {
                    return self.clone();
                };
                self.apply_current_at(
                    self.topology.local + Did::power_of_two(request.slot_index()),
                    expires_at_ms.max(self.now_ms),
                )
            }
            FingerRetryAction::Cancel => {
                let Some(request) = self.current_request() else {
                    return self.clone();
                };
                let output = step(
                    &self.topology,
                    TopologyEvent::CancelFinger {
                        request,
                        now_ms: self.now_ms,
                    },
                    DEFAULT_SUCCESSOR_CAPACITY,
                );
                let mut next = self.clone();
                next.topology = output.state;
                next.delayed_reports.push(request);
                next
            }
            FingerRetryAction::Lose => self.clone(),
            FingerRetryAction::Duplicate => {
                let Some(request) = self.delayed_reports.last().copied() else {
                    return self.clone();
                };
                let output = step(
                    &self.topology,
                    TopologyEvent::ApplyFinger {
                        request,
                        successor: Did::from(2u32),
                        now_ms: self.now_ms,
                    },
                    DEFAULT_SUCCESSOR_CAPACITY,
                );
                let mut next = self.clone();
                next.topology = output.state;
                next
            }
            FingerRetryAction::TopologyChange => {
                let event = match self.next_topology_mutation {
                    0 => TopologyEvent::Join {
                        peer: Did::from(4u32),
                    },
                    1 => TopologyEvent::Admit {
                        peer: Did::from(6u32),
                        fixed_fingers: Vec::new(),
                        now_ms: self.now_ms,
                    },
                    2 => TopologyEvent::UpdateSuccessor {
                        successor: Did::from(16u32),
                    },
                    3 => TopologyEvent::Stabilize {
                        successors: vec![Did::from(32u32)],
                        predecessor: Some(Did::from(2u32)),
                    },
                    _ => TopologyEvent::Remove {
                        peer: self
                            .topology
                            .successors
                            .first()
                            .copied()
                            .unwrap_or(Did::from(4u32)),
                        successor: SuccessorRemoval::Preserve,
                    },
                };
                let output = step(&self.topology, event, DEFAULT_SUCCESSOR_CAPACITY);
                let mut next = self.clone();
                next.topology = output.state;
                next.next_topology_mutation = self.next_topology_mutation.saturating_add(1) % 5;
                next
            }
            FingerRetryAction::Restart => {
                let mut next = self.clone();
                next.topology = TopologyState::new(
                    self.topology.local,
                    self.topology.successors.clone(),
                    self.topology.predecessor,
                    self.topology.fingers.clone(),
                    self.topology.fix_finger_index,
                );
                if let Some(request) = self.current_request() {
                    next.delayed_reports.push(request);
                }
                next.now_ms = 0;
                next.run_generation = next.run_generation.saturating_add(1);
                next
            }
        }
    }

    fn preserves_emission_interval(&self) -> bool {
        self.emissions.windows(2).all(|window| {
            window.first().zip(window.get(1)).is_some_and(
                |((_, earlier, earlier_run), (_, later, later_run))| {
                    earlier_run != later_run
                        || later.saturating_sub(*earlier) >= FINGER_LOOKUP_MIN_INTERVAL_MS
                },
            )
        })
    }

    fn uses_unique_request_ids(&self) -> bool {
        let identities = self
            .emissions
            .iter()
            .map(|(request_id, _, _)| *request_id)
            .collect::<BTreeSet<_>>();
        identities.len() == self.emissions.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FingerRetryAction {
    AdvanceBeforeDeadline,
    AdvanceToDeadline,
    Progress,
    Invalid,
    LateReport,
    Cancel,
    Lose,
    Duplicate,
    TopologyChange,
    Restart,
}

const ACTIONS: [FingerRetryAction; 10] = [
    FingerRetryAction::AdvanceBeforeDeadline,
    FingerRetryAction::AdvanceToDeadline,
    FingerRetryAction::Progress,
    FingerRetryAction::Invalid,
    FingerRetryAction::LateReport,
    FingerRetryAction::Cancel,
    FingerRetryAction::Lose,
    FingerRetryAction::Duplicate,
    FingerRetryAction::TopologyChange,
    FingerRetryAction::Restart,
];

#[test]
fn test_topology_event_formal_scope_manifest_covers_cross_domain_invalidations() {
    let request = FingerFixRequest::new(0, uuid::Uuid::from_u128(1)).unwrap_or(FingerFixRequest {
        slot: u16::MAX,
        request_id: uuid::Uuid::nil(),
    });
    let local = Did::from(0u32);
    let peer = Did::from(1u32);
    let events = [
        TopologyEvent::Join { peer },
        TopologyEvent::Admit {
            peer,
            fixed_fingers: vec![ConditionalFingerUpdate { request }],
            now_ms: 1,
        },
        TopologyEvent::Remove {
            peer,
            successor: SuccessorRemoval::Preserve,
        },
        TopologyEvent::UpdateSuccessor { successor: peer },
        TopologyEvent::Notify { predecessor: peer },
        TopologyEvent::Stabilize {
            successors: vec![peer],
            predecessor: Some(peer),
        },
        TopologyEvent::BeginFingerRevalidation,
        TopologyEvent::AdvanceFingerConvergence {
            now_ms: 1,
            request_id: uuid::Uuid::from_u128(2),
        },
        TopologyEvent::ApplyFinger {
            request,
            successor: local,
            now_ms: 1,
        },
        TopologyEvent::CancelFinger { request, now_ms: 1 },
    ];

    for event in &events {
        assert!(!formal_model_scopes(event).is_empty());
    }
    assert_eq!(formal_model_scopes(&events[0]), DHT_AND_FINGER);
    assert_eq!(formal_model_scopes(&events[1]), DHT_AND_FINGER);
    assert_eq!(formal_model_scopes(&events[4]), DHT_ONLY);
    assert_eq!(formal_model_scopes(&events[7]), FINGER_ONLY);
}

#[test]
fn test_production_finger_retry_transition_preserves_bounded_resource_laws() {
    let mut seen = vec![FingerRetryState::initial()];
    let mut frontier = seen.clone();
    for _ in 0..MODEL_DEPTH {
        let mut next_frontier = Vec::new();
        for state in frontier {
            for action in ACTIONS {
                let next = state.transition(action);
                assert!(next.preserves_emission_interval());
                assert!(next.uses_unique_request_ids());

                if matches!(action, FingerRetryAction::AdvanceBeforeDeadline) {
                    assert_eq!(next.emissions, state.emissions);
                }
                if matches!(action, FingerRetryAction::Duplicate) {
                    assert_eq!(next.topology, state.topology);
                }
                if matches!(
                    action,
                    FingerRetryAction::Invalid
                        | FingerRetryAction::LateReport
                        | FingerRetryAction::Cancel
                ) && state.current_request().is_some()
                    && next.current_request().is_none()
                {
                    let projection = next.topology.finger_convergence_projection();
                    assert_eq!(
                        projection.retry_not_before_ms,
                        Some(
                            next.now_ms.saturating_add(finger_lookup_backoff_ms(
                                projection.failure_streak
                            ))
                        )
                    );
                }
                if matches!(action, FingerRetryAction::LateReport)
                    && state.current_request().is_some()
                {
                    assert_eq!(next.topology.fingers, state.topology.fingers);
                }

                if !seen.iter().any(|visited| visited == &next) {
                    seen.push(next.clone());
                    next_frontier.push(next);
                }
            }
        }
        frontier = next_frontier;
    }
}

#[test]
fn test_restart_delayed_report_cannot_match_the_next_production_request() {
    let issued = FingerRetryState::initial().transition(FingerRetryAction::AdvanceToDeadline);
    let old_request = issued.current_request();
    let restarted = issued.transition(FingerRetryAction::Restart);
    let reissued = restarted.transition(FingerRetryAction::AdvanceToDeadline);

    assert!(old_request.is_some());
    assert!(reissued.current_request().is_some());
    assert_ne!(old_request, reissued.current_request());
    assert_eq!(
        reissued.transition(FingerRetryAction::Duplicate).topology,
        reissued.topology
    );
}
