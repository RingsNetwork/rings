//! Finite model checking over the production finger-convergence transition.
//!
//! State variables are the real [`TopologyState`], process-monotonic time,
//! emitted request identities, delayed reports, and the next identity supplied
//! by the effect boundary. The environment may advance before or to a
//! deadline, deliver progress, invalid evidence, or a report exactly at its
//! expiry, cancel, lose, duplicate, change topology, or restart.
//! Stabilization additionally models the requested/claimed/consumed phases and
//! the bounded connection effects that may occur between claim and commit.
//!
//! Safety does not assume eventual delivery. Liveness is conditional on a fair
//! scheduler and eventual valid delivery. The checker executes
//! [`crate::dht::topology::step`] itself, so production and model semantics
//! cannot drift behind a scope manifest.

use std::collections::BTreeSet;
use std::collections::HashSet;

use crate::dht::finger::finger_lookup_backoff_ms;
use crate::dht::finger::finger_proof_end;
use crate::dht::finger::FINGER_LOOKUP_MIN_INTERVAL_MS;
use crate::dht::finger_awaiting_report_deadline_for_test;
use crate::dht::topology::step;
use crate::dht::topology::successor_head;
use crate::dht::topology::ConditionalFingerUpdate;
use crate::dht::topology::StabilizationConnectionPlan;
use crate::dht::topology::StabilizationConnectionStep;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyAction;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
use crate::dht::Did;
use crate::dht::FingerFixRequest;

const MODEL_DEPTH: usize = 9;
const MAX_STABILIZATION_CONNECTION_EFFECTS: u8 =
    (DEFAULT_SUCCESSOR_CAPACITY as u8).saturating_add(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormalModelScope {
    DhtTopology,
    FingerRetry,
}

const DHT_ONLY: &[FormalModelScope] = &[FormalModelScope::DhtTopology];
const FINGER_ONLY: &[FormalModelScope] = &[FormalModelScope::FingerRetry];
const DHT_AND_FINGER: &[FormalModelScope] =
    &[FormalModelScope::DhtTopology, FormalModelScope::FingerRetry];

/// Scope-routing manifest for model composition. This exhaustive match keeps
/// event classification current, but is deliberately not treated as a model
/// completeness proof. Concrete action coverage and network effects are
/// checked by the transition explorations and production-path tests below.
fn formal_model_scopes(event: &TopologyEvent) -> &'static [FormalModelScope] {
    match event {
        TopologyEvent::Join { .. }
        | TopologyEvent::Admit { .. }
        | TopologyEvent::Remove { .. }
        | TopologyEvent::UpdateSuccessor { .. }
        | TopologyEvent::Stabilize { .. } => DHT_AND_FINGER,
        TopologyEvent::Notify { .. }
        | TopologyEvent::BeginStabilize { .. }
        | TopologyEvent::ClaimStabilize { .. }
        | TopologyEvent::CancelStabilize { .. } => DHT_ONLY,
        TopologyEvent::BeginFingerRevalidation
        | TopologyEvent::AdvanceFingerConvergence { .. }
        | TopologyEvent::ApplyFinger { .. }
        | TopologyEvent::DeferFinger { .. }
        | TopologyEvent::CancelFinger { .. } => FINGER_ONLY,
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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
                peer: Did::from(1u32),
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
        let projection = self.topology.finger_convergence_projection();
        projection.in_flight.or(projection.deferred)
    }

    fn in_flight_request(&self) -> Option<FingerFixRequest> {
        self.topology.finger_convergence_projection().in_flight
    }

    fn deferred_request(&self) -> Option<FingerFixRequest> {
        self.topology.finger_convergence_projection().deferred
    }

    fn next_deadline_ms(&self) -> u64 {
        let projection = self.topology.finger_convergence_projection();
        if let Some(expires_at_ms) = projection.expires_at_ms {
            return expires_at_ms.max(self.now_ms);
        }
        if let Some(expires_at_ms) = projection.deferred_expires_at_ms {
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
        let Some(request) = self.in_flight_request() else {
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

    fn defer_current(&self) -> Self {
        let Some(request) = self.in_flight_request() else {
            return self.clone();
        };
        let successor = self.topology.local + Did::power_of_two(request.slot_index());
        let output = step(
            &self.topology,
            TopologyEvent::DeferFinger {
                request,
                successor,
                now_ms: self.now_ms,
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        next.delayed_reports.push(request);
        next
    }

    fn admit_deferred(&self) -> Self {
        let Some(request) = self.deferred_request() else {
            return self.clone();
        };
        let peer = self.topology.local + Did::power_of_two(request.slot_index());
        let output = step(
            &self.topology,
            TopologyEvent::Admit {
                peer,
                fixed_fingers: vec![ConditionalFingerUpdate { request }],
                now_ms: self.now_ms,
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        next
    }

    fn advance_before_deadline(&self) -> Self {
        let deadline = self.next_deadline_ms();
        if deadline > self.now_ms {
            self.advance_at(deadline.saturating_sub(1))
        } else {
            self.clone()
        }
    }

    fn apply_progress(&self) -> Self {
        let Some(request) = self.in_flight_request() else {
            return self.clone();
        };
        self.apply_current(self.topology.local + Did::power_of_two(request.slot_index()))
    }

    fn apply_invalid_report(&self) -> Self {
        let Some(request) = self.in_flight_request() else {
            return self.clone();
        };
        let Some(previous_slot) = request.slot_index().checked_sub(1) else {
            return self.clone();
        };
        self.apply_current(self.topology.local + Did::power_of_two(previous_slot))
    }

    fn apply_late_report(&self) -> Self {
        let projection = self.topology.finger_convergence_projection();
        let (Some(request), Some(expires_at_ms)) = (projection.in_flight, projection.expires_at_ms)
        else {
            return self.clone();
        };
        self.apply_current_at(
            self.topology.local + Did::power_of_two(request.slot_index()),
            expires_at_ms.max(self.now_ms),
        )
    }

    fn cancel_current(&self) -> Self {
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

    fn deliver_duplicate(&self) -> Self {
        let Some(request) = self.delayed_reports.last().copied() else {
            return self.clone();
        };
        let output = step(
            &self.topology,
            TopologyEvent::DeferFinger {
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

    fn change_topology(&self) -> Self {
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
                reporter: self
                    .topology
                    .successors
                    .first()
                    .copied()
                    .unwrap_or(Did::from(8u32)),
                request_id: None,
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

    fn restart(&self) -> Self {
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

    fn transition(&self, action: FingerRetryAction) -> Self {
        match action {
            FingerRetryAction::AdvanceBeforeDeadline => self.advance_before_deadline(),
            FingerRetryAction::AdvanceToDeadline => self.advance_at(self.next_deadline_ms()),
            FingerRetryAction::Progress => self.apply_progress(),
            FingerRetryAction::Invalid => self.apply_invalid_report(),
            FingerRetryAction::LateReport => self.apply_late_report(),
            FingerRetryAction::Cancel => self.cancel_current(),
            FingerRetryAction::Defer => self.defer_current(),
            FingerRetryAction::AdmitDeferred => self.admit_deferred(),
            FingerRetryAction::Lose => self.clone(),
            FingerRetryAction::Duplicate => self.deliver_duplicate(),
            FingerRetryAction::TopologyChange => self.change_topology(),
            FingerRetryAction::Restart => self.restart(),
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
    Defer,
    AdmitDeferred,
    Lose,
    Duplicate,
    TopologyChange,
    Restart,
}

const ACTIONS: [FingerRetryAction; 12] = [
    FingerRetryAction::AdvanceBeforeDeadline,
    FingerRetryAction::AdvanceToDeadline,
    FingerRetryAction::Progress,
    FingerRetryAction::Invalid,
    FingerRetryAction::LateReport,
    FingerRetryAction::Cancel,
    FingerRetryAction::Defer,
    FingerRetryAction::AdmitDeferred,
    FingerRetryAction::Lose,
    FingerRetryAction::Duplicate,
    FingerRetryAction::TopologyChange,
    FingerRetryAction::Restart,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StabilizationAction {
    Begin,
    ClaimCurrentProof,
    ReserveCurrentCandidate,
    ExecuteReservedCandidate,
    AttemptSupersededCandidate,
    CancelCurrent,
    CompleteCurrentProof,
    DeliverSupersededProof,
    MoveSuccessorHead,
}

const STABILIZATION_ACTIONS: [StabilizationAction; 9] = [
    StabilizationAction::Begin,
    StabilizationAction::ClaimCurrentProof,
    StabilizationAction::ReserveCurrentCandidate,
    StabilizationAction::ExecuteReservedCandidate,
    StabilizationAction::AttemptSupersededCandidate,
    StabilizationAction::CancelCurrent,
    StabilizationAction::CompleteCurrentProof,
    StabilizationAction::DeliverSupersededProof,
    StabilizationAction::MoveSuccessorHead,
];

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StabilizationModelState {
    topology: TopologyState,
    current: Option<(Did, uuid::Uuid, bool)>,
    current_plan: Option<StabilizationConnectionPlan>,
    superseded: Vec<(Did, uuid::Uuid)>,
    superseded_plans: Vec<StabilizationConnectionPlan>,
    reserved_connection_effects: Vec<uuid::Uuid>,
    connection_effects: Vec<(uuid::Uuid, u8)>,
    next_request_id: u128,
}

impl StabilizationModelState {
    fn initial() -> Self {
        let local = Did::from(0u32);
        let joined = step(
            &TopologyState::new(local, Vec::new(), None, vec![None; 4], 0),
            TopologyEvent::Join {
                peer: Did::from(4u32),
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        Self {
            topology: joined.state,
            current: None,
            current_plan: None,
            superseded: Vec::new(),
            superseded_plans: Vec::new(),
            reserved_connection_effects: Vec::new(),
            connection_effects: Vec::new(),
            next_request_id: 1,
        }
    }

    fn begin(&self) -> Self {
        let request_id = uuid::Uuid::from_u128(self.next_request_id);
        let output = step(
            &self.topology,
            TopologyEvent::BeginStabilize { request_id },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let issued = output.actions.iter().find_map(|action| match action {
            TopologyAction::QuerySuccessorTopology {
                successor,
                request_id,
            } => Some((*successor, *request_id)),
            _ => None,
        });
        let mut next = self.clone();
        next.topology = output.state;
        if let Some((reporter, request_id, _)) = self.current {
            next.superseded.push((reporter, request_id));
        }
        if let Some(plan) = self.current_plan.clone() {
            next.superseded_plans.push(plan);
        }
        next.current = issued.map(|(reporter, request_id)| (reporter, request_id, false));
        next.current_plan = None;
        next.next_request_id = self.next_request_id.saturating_add(1);
        next
    }

    fn claim_current(&self) -> Self {
        let Some((reporter, request_id, false)) = self.current else {
            return self.clone();
        };
        let output = step(
            &self.topology,
            TopologyEvent::ClaimStabilize {
                reporter,
                request_id,
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        next.current = Some((reporter, request_id, true));
        next.current_plan = Some(StabilizationConnectionPlan::new(
            reporter,
            request_id,
            (1..=10u32).map(Did::from),
            self.topology.local,
            DEFAULT_SUCCESSOR_CAPACITY,
        ));
        next
    }

    fn complete(&self, request: Option<(Did, uuid::Uuid, bool)>) -> Self {
        let Some((reporter, request_id, true)) = request else {
            return self.clone();
        };
        if self.reserved_connection_effects.contains(&request_id) {
            return self.clone();
        }
        self.deliver((reporter, request_id))
    }

    fn reserve_current_candidate(&self) -> Self {
        let mut next = self.clone();
        let Some((_, current_request_id, true)) = next.current else {
            return next;
        };
        if next
            .reserved_connection_effects
            .contains(&current_request_id)
        {
            return next;
        }
        let Some(plan) = next.current_plan.as_mut() else {
            return next;
        };
        if let StabilizationConnectionStep::Connect { request_id, .. } =
            plan.advance(&next.topology)
        {
            next.reserved_connection_effects.push(request_id);
        }
        next
    }

    fn execute_reserved_candidate(&self) -> Self {
        let mut next = self.clone();
        if let Some(request_id) = next.reserved_connection_effects.pop() {
            next.record_connection_effect(request_id);
        }
        next
    }

    fn attempt_superseded_candidate(&self) -> Self {
        let mut next = self.clone();
        let Some(plan) = next.superseded_plans.last_mut() else {
            return next;
        };
        if let StabilizationConnectionStep::Connect { request_id, .. } =
            plan.advance(&next.topology)
        {
            next.record_connection_effect(request_id);
        }
        next
    }

    fn record_connection_effect(&mut self, request_id: uuid::Uuid) {
        match self
            .connection_effects
            .iter_mut()
            .find(|(id, _)| *id == request_id)
        {
            Some((_, count)) => *count = count.saturating_add(1),
            None => self.connection_effects.push((request_id, 1)),
        }
    }

    fn cancel_current(&self) -> Self {
        let Some((reporter, request_id, _)) = self.current else {
            return self.clone();
        };
        let output = step(
            &self.topology,
            TopologyEvent::CancelStabilize { request_id },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        next.superseded.push((reporter, request_id));
        if let Some(plan) = self.current_plan.clone() {
            next.superseded_plans.push(plan);
        }
        next.current = None;
        next.current_plan = None;
        next
    }

    fn deliver(&self, request: (Did, uuid::Uuid)) -> Self {
        let (reporter, request_id) = request;
        let output = step(
            &self.topology,
            TopologyEvent::Stabilize {
                reporter,
                request_id: Some(request_id),
                successors: vec![reporter],
                predecessor: Some(self.topology.local),
            },
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        let mut next = self.clone();
        next.topology = output.state;
        if self.current == Some((reporter, request_id, true)) {
            next.current = None;
            next.current_plan = None;
        }
        next
    }

    fn transition(&self, action: StabilizationAction) -> Self {
        match action {
            StabilizationAction::Begin => self.begin(),
            StabilizationAction::ClaimCurrentProof => self.claim_current(),
            StabilizationAction::ReserveCurrentCandidate => self.reserve_current_candidate(),
            StabilizationAction::ExecuteReservedCandidate => self.execute_reserved_candidate(),
            StabilizationAction::AttemptSupersededCandidate => self.attempt_superseded_candidate(),
            StabilizationAction::CancelCurrent => self.cancel_current(),
            StabilizationAction::CompleteCurrentProof => self.complete(self.current),
            StabilizationAction::DeliverSupersededProof => self
                .superseded
                .last()
                .copied()
                .map_or_else(|| self.clone(), |request| self.deliver(request)),
            StabilizationAction::MoveSuccessorHead => {
                let previous_head = successor_head(&self.topology);
                let output = step(
                    &self.topology,
                    TopologyEvent::Join {
                        peer: Did::from(2u32),
                    },
                    DEFAULT_SUCCESSOR_CAPACITY,
                );
                let mut next = self.clone();
                next.topology = output.state;
                if successor_head(&next.topology) != previous_head {
                    if let Some((reporter, request_id, _)) = self.current {
                        next.superseded.push((reporter, request_id));
                    }
                    if let Some(plan) = self.current_plan.clone() {
                        next.superseded_plans.push(plan);
                    }
                    next.current = None;
                    next.current_plan = None;
                }
                next
            }
        }
    }
}

#[test]
fn test_topology_event_scope_routing_is_exhaustive() {
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
        TopologyEvent::BeginStabilize {
            request_id: uuid::Uuid::from_u128(3),
        },
        TopologyEvent::ClaimStabilize {
            reporter: peer,
            request_id: uuid::Uuid::from_u128(3),
        },
        TopologyEvent::Stabilize {
            reporter: peer,
            request_id: None,
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
        TopologyEvent::DeferFinger {
            request,
            successor: local,
            now_ms: 1,
        },
        TopologyEvent::CancelFinger { request, now_ms: 1 },
        TopologyEvent::CancelStabilize {
            request_id: uuid::Uuid::from_u128(3),
        },
    ];

    for event in &events {
        assert!(!formal_model_scopes(event).is_empty());
    }
    assert_eq!(formal_model_scopes(&events[0]), DHT_AND_FINGER);
    assert_eq!(formal_model_scopes(&events[1]), DHT_AND_FINGER);
    assert_eq!(formal_model_scopes(&events[4]), DHT_ONLY);
    assert_eq!(formal_model_scopes(&events[9]), FINGER_ONLY);
}

#[test]
fn test_production_stabilization_effect_plan_caps_and_stops_after_supersession() {
    let claimed = StabilizationModelState::initial()
        .transition(StabilizationAction::Begin)
        .transition(StabilizationAction::ClaimCurrentProof);
    let mut advanced = claimed;
    for expected in 1..=MAX_STABILIZATION_CONNECTION_EFFECTS {
        advanced = advanced
            .transition(StabilizationAction::ReserveCurrentCandidate)
            .transition(StabilizationAction::ExecuteReservedCandidate);
        assert_eq!(advanced.connection_effects[0].1, expected);
    }

    let exhausted = advanced.transition(StabilizationAction::ReserveCurrentCandidate);
    assert_eq!(exhausted.connection_effects, advanced.connection_effects);

    let reserved = StabilizationModelState::initial()
        .transition(StabilizationAction::Begin)
        .transition(StabilizationAction::ClaimCurrentProof)
        .transition(StabilizationAction::ReserveCurrentCandidate);
    let superseded = reserved.transition(StabilizationAction::Begin);
    let permitted_after_supersession =
        superseded.transition(StabilizationAction::ExecuteReservedCandidate);
    assert_eq!(permitted_after_supersession.connection_effects[0].1, 1);
    let stale_attempt =
        permitted_after_supersession.transition(StabilizationAction::AttemptSupersededCandidate);
    assert_eq!(
        stale_attempt.connection_effects,
        permitted_after_supersession.connection_effects
    );

    let cancelled = superseded.transition(StabilizationAction::CancelCurrent);
    assert_eq!(cancelled.current, None);
    assert_eq!(cancelled.current_plan, None);
}

#[test]
fn test_production_stabilization_tokens_gate_verified_range_proofs() {
    let initial = StabilizationModelState::initial();
    let mut seen = HashSet::from([initial.clone()]);
    let mut frontier = vec![initial];

    for _ in 0..6 {
        let mut next_frontier = Vec::new();
        for state in frontier {
            for action in STABILIZATION_ACTIONS {
                let next = state.transition(action);
                if matches!(action, StabilizationAction::DeliverSupersededProof)
                    && state.superseded.last().is_some()
                {
                    assert_eq!(next.topology, state.topology);
                }
                if matches!(action, StabilizationAction::AttemptSupersededCandidate) {
                    assert_eq!(next.connection_effects, state.connection_effects);
                }
                assert_eq!(
                    next.reserved_connection_effects
                        .iter()
                        .copied()
                        .collect::<BTreeSet<_>>()
                        .len(),
                    next.reserved_connection_effects.len()
                );
                assert!(next
                    .connection_effects
                    .iter()
                    .all(|(_, count)| *count <= MAX_STABILIZATION_CONNECTION_EFFECTS));
                if matches!(action, StabilizationAction::CompleteCurrentProof) {
                    if let Some((reporter, request_id, true)) = state.current {
                        if !state.reserved_connection_effects.contains(&request_id) {
                            let end = finger_proof_end(
                                state.topology.local,
                                reporter,
                                0,
                                state.topology.fingers.len(),
                            )
                            .unwrap_or(0);
                            assert!(next
                                .topology
                                .finger_convergence_projection()
                                .verified
                                .iter()
                                .take(end.saturating_add(1))
                                .all(|verified| *verified));
                        }
                    }
                }
                if seen.insert(next.clone()) {
                    next_frontier.push(next);
                }
            }
        }
        frontier = next_frontier;
    }
}

#[test]
fn test_listener_restart_preserves_the_lookup_remaining_timeout() {
    let ring_now_ms = 3_600_000;
    let listener_now_ms = 0;
    let mut state = FingerRetryState::initial().advance_at(ring_now_ms);
    let status = state
        .topology
        .finger_convergence_status(ring_now_ms.saturating_add(250));
    let deadline = finger_awaiting_report_deadline_for_test(listener_now_ms, status);

    assert_eq!(deadline, 9_750);

    state.now_ms = ring_now_ms.saturating_add(250);
    let restarted_again = finger_awaiting_report_deadline_for_test(
        listener_now_ms,
        state.topology.finger_convergence_status(state.now_ms),
    );
    assert_eq!(restarted_again, 9_750);
}

#[test]
fn test_production_finger_retry_transition_preserves_bounded_resource_laws() {
    let initial = FingerRetryState::initial();
    let mut seen = HashSet::from([initial.clone()]);
    let mut frontier = vec![initial];
    for _ in 0..MODEL_DEPTH {
        let mut next_frontier = Vec::new();
        for state in frontier {
            for action in ACTIONS {
                let next = state.transition(action);
                assert!(next.preserves_emission_interval());
                assert!(next.uses_unique_request_ids());
                let projection = next.topology.finger_convergence_projection();
                assert!(projection.in_flight.is_none() || projection.deferred.is_none());

                if matches!(action, FingerRetryAction::AdvanceBeforeDeadline) {
                    assert_eq!(next.emissions, state.emissions);
                }
                if matches!(action, FingerRetryAction::AdvanceToDeadline)
                    && state.deferred_request().is_some()
                {
                    assert_eq!(next.deferred_request(), None);
                    assert!(
                        next.topology.finger_convergence_projection().failure_streak
                            > state
                                .topology
                                .finger_convergence_projection()
                                .failure_streak
                    );
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
                    && state.in_flight_request().is_some()
                {
                    assert_eq!(next.topology.fingers, state.topology.fingers);
                }
                if matches!(action, FingerRetryAction::Defer) && state.in_flight_request().is_some()
                {
                    assert_eq!(next.in_flight_request(), None);
                    assert_eq!(next.deferred_request(), state.in_flight_request());
                    assert_eq!(
                        projection.failure_streak,
                        state
                            .topology
                            .finger_convergence_projection()
                            .failure_streak
                    );
                }
                if matches!(action, FingerRetryAction::AdmitDeferred)
                    && state.deferred_request().is_some()
                {
                    assert_eq!(next.deferred_request(), None);
                }

                if seen.insert(next.clone()) {
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
