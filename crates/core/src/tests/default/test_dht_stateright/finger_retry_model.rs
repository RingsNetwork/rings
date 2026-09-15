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
//!
//! Model exploration flow:
//!
//! ```text
//! initial production state
//!          |
//!          v
//! choose environment action
//!          |
//!          v
//! execute reducer or effect plan
//!          |
//!          v
//! check correlation, timing, bounds
//!          |
//!    +-----+-----+
//!    |           |
//! new state   duplicate
//!    |           |
//!    v           v
//! enqueue      discard
//! ```

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

/// Search depth covering timeout, admission, retry, churn, and restart.
///
/// The bound reaches every modeled transition class while keeping the finite
/// state space tractable.
const MODEL_DEPTH: usize = 9;
/// Maximum connection effects owned by one claimed stabilization proof.
///
/// A report may admit the successor list plus one predecessor; accepting more
/// would violate bounded fan-out.
const MAX_STABILIZATION_CONNECTION_EFFECTS: u8 =
    (DEFAULT_SUCCESSOR_CAPACITY as u8).saturating_add(1);

/// Complete finite-checker state for adversarial finger retry schedules.
///
/// Production topology is retained verbatim; other fields model environment
/// time, replayable traffic, effect identities, and process epochs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct FingerRetryState {
    /// Production topology and convergence transition under verification.
    topology: TopologyState,
    /// Monotonic model time in the same milliseconds as topology events.
    now_ms: u64,
    /// Emitted `(request_id, issued_at_ms, process_generation)` witnesses.
    emissions: Vec<(uuid::Uuid, u64, u64)>,
    /// Retired requests that the environment may replay as delayed traffic.
    delayed_reports: Vec<FingerFixRequest>,
    /// Next UUID payload supplied by the effect boundary, not topology state.
    next_request_id: u128,
    /// Cursor selecting the next representative mutation in the churn cycle.
    next_topology_mutation: u8,
    /// Process epoch separating independent monotonic-clock generations.
    run_generation: u64,
}

impl FingerRetryState {
    /// Initial state contains one admitted successor so finger convergence is
    /// runnable without modeling unrelated bootstrap liveness.
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

    /// Return the request owning report or admission progress.
    ///
    /// This reads the public projection each time, avoiding shadow ownership state.
    fn current_request(&self) -> Option<FingerFixRequest> {
        let projection = self.topology.finger_convergence_projection();
        projection.in_flight.or(projection.deferred)
    }

    /// Return the request currently waiting for a lookup report.
    ///
    /// `None` means production exposes no in-flight report owner.
    fn in_flight_request(&self) -> Option<FingerFixRequest> {
        self.topology.finger_convergence_projection().in_flight
    }

    /// Return the request whose proof is retained for transport admission.
    ///
    /// This phase is mutually exclusive with an in-flight report owner.
    fn deferred_request(&self) -> Option<FingerFixRequest> {
        self.topology.finger_convergence_projection().deferred
    }

    /// Earliest scheduler-visible instant across report expiry, retained proof
    /// lease expiry, retry backoff, and the per-process emission interval.
    ///
    /// The result never precedes current model time, so an action can advance
    /// exactly to a meaningful production boundary.
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

    /// Advance the production transition at a chosen model timestamp.
    ///
    /// Only an emitted lookup consumes the offered UUID and appends an emission
    /// witness; dormant or cleanup-only transitions preserve identity state.
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
        // Only an emitted lookup consumes a fresh UUID. A due transition that
        // merely waits or cleans up must not burn request identity space.
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

    /// Deliver a current report at the current model timestamp.
    ///
    /// The environment supplies successor evidence while ownership is read from
    /// the production convergence projection.
    fn apply_current(&self, successor: Did) -> Self {
        self.apply_current_at(successor, self.now_ms)
    }

    /// Delivers a report for the exact in-flight request and records that same
    /// request as replayable delayed traffic.
    ///
    /// Retaining the token enables duplicate and post-restart delivery actions.
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

    /// Move the current valid report into the admission-lease phase.
    ///
    /// Exact lower-bound evidence keeps rejection attributable to ownership or
    /// timing rather than an invalid successor value.
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

    /// Complete pending admission and apply the retained proof atomically.
    ///
    /// The peer is reconstructed from the deferred slot so the same proof enters
    /// and leaves admission ownership.
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

    /// Advance immediately before the next scheduler deadline when possible.
    ///
    /// This witnesses that expiry, retry, and rate-limit work cannot happen one
    /// millisecond before its production boundary.
    fn advance_before_deadline(&self) -> Self {
        let deadline = self.next_deadline_ms();
        if deadline > self.now_ms {
            self.advance_at(deadline.saturating_sub(1))
        } else {
            self.clone()
        }
    }

    /// Deliver evidence that exactly proves the requested lower-bound slot.
    ///
    /// This successful-progress action should reset failure backoff while keeping
    /// request correlation intact.
    fn apply_progress(&self) -> Self {
        let Some(request) = self.in_flight_request() else {
            return self.clone();
        };
        self.apply_current(self.topology.local + Did::power_of_two(request.slot_index()))
    }

    /// Valid correlation with an invalid range: it should fail like network
    /// progress failure without proving the requested finger slot.
    ///
    /// The preceding power-of-two boundary keeps the token valid while making
    /// successor evidence insufficient for the requested range.
    fn apply_invalid_report(&self) -> Self {
        let Some(request) = self.in_flight_request() else {
            return self.clone();
        };
        let Some(previous_slot) = request.slot_index().checked_sub(1) else {
            return self.clone();
        };
        self.apply_current(self.topology.local + Did::power_of_two(previous_slot))
    }

    /// Deliver current evidence exactly at its inclusive expiry boundary.
    ///
    /// Production must classify it as stale, preserve fingers, and perform timeout
    /// cleanup without relying on a prior scheduler poll.
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

    /// Cancel the active request and make its old token replayable.
    ///
    /// Cancellation covers report and admission ownership and enters failure
    /// backoff without granting later authority to the retired token.
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

    /// Replay the most recent delayed request as duplicate or stale traffic.
    ///
    /// Correlation must reject the token before the synthetic successor can mutate
    /// topology or finger state.
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

    /// Cycles through topology events that can invalidate finger evidence while
    /// staying independent from retry failure accounting.
    ///
    /// The bounded cycle covers join, admission, successor change, stabilization,
    /// and removal without unboundedly expanding the action alphabet.
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
            3 => return self.stabilize_from_head(),
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

    /// Apply one complete correlated stabilization round from the current head.
    ///
    /// Production accepts a report only through `BeginStabilize`, `ClaimStabilize`
    /// and a `Stabilize` carrying the same token, so the mutation runs all three.
    /// Without a head there is no round and the cycle simply advances.
    fn stabilize_from_head(&self) -> Self {
        let mut next = self.clone();
        next.next_topology_mutation = self.next_topology_mutation.saturating_add(1) % 5;
        let Some(reporter) = successor_head(&self.topology) else {
            return next;
        };
        let request_id = uuid::Uuid::from_u128(self.next_request_id);
        next.next_request_id = self.next_request_id.saturating_add(1);
        for event in [
            TopologyEvent::BeginStabilize { request_id },
            TopologyEvent::ClaimStabilize {
                reporter,
                request_id,
            },
            TopologyEvent::Stabilize {
                reporter,
                request_id,
                successors: vec![Did::from(32u32)],
                predecessor: Some(Did::from(2u32)),
            },
        ] {
            next.topology = step(&next.topology, event, DEFAULT_SUCCESSOR_CAPACITY).state;
        }
        next
    }

    /// Process restart rebuilds the ring exactly as production does: nothing
    /// about topology or convergence is persisted, so the node starts from an
    /// empty table and rejoins its previous successor head as its seed. Any
    /// delayed report from before the restart must then prove its old UUID
    /// against the new process.
    ///
    /// Model time resets and the process epoch advances while identity generation
    /// and replay history remain continuous.
    fn restart(&self) -> Self {
        let mut next = self.clone();
        let fresh = TopologyState::new(
            self.topology.local,
            Vec::new(),
            None,
            vec![None; self.topology.fingers.len()],
            0,
        );
        next.topology = match successor_head(&self.topology) {
            Some(seed) => {
                step(
                    &fresh,
                    TopologyEvent::Join { peer: seed },
                    DEFAULT_SUCCESSOR_CAPACITY,
                )
                .state
            }
            None => fresh,
        };
        if let Some(request) = self.current_request() {
            next.delayed_reports.push(request);
        }
        next.now_ms = 0;
        next.run_generation = next.run_generation.saturating_add(1);
        next
    }

    /// Interpret one adversarial retry action as a pure state transition.
    ///
    /// Deterministic state-to-state mapping permits breadth-first exploration and
    /// duplicate-state elimination.
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

    /// Check minimum lookup spacing within every process generation.
    ///
    /// Restart begins a new clock domain; adjacent emissions within one domain must
    /// remain at least one production interval apart.
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

    /// Check that emitted lookup UUIDs are never reused across restarts.
    ///
    /// Set cardinality must equal the complete emission log length.
    fn uses_unique_request_ids(&self) -> bool {
        let identities = self
            .emissions
            .iter()
            .map(|(request_id, _, _)| *request_id)
            .collect::<BTreeSet<_>>();
        identities.len() == self.emissions.len()
    }
}

/// Environment actions for the adversarial retry scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FingerRetryAction {
    /// Poll one millisecond before the next scheduler boundary.
    AdvanceBeforeDeadline,
    /// Advance exactly to the next expiry, retry, or rate-limit deadline.
    AdvanceToDeadline,
    /// Deliver valid evidence for the requested finger range.
    Progress,
    /// Deliver correlated evidence that cannot prove the requested range.
    Invalid,
    /// Deliver valid evidence exactly when its report lease expires.
    LateReport,
    /// Cancel the request owning report or admission progress.
    Cancel,
    /// Retain a valid proof while its candidate waits for admission.
    Defer,
    /// Admit the deferred candidate and consume its proof.
    AdmitDeferred,
    /// Model a scheduler turn with no delivery or timeout progress.
    Lose,
    /// Replay the most recently retired or consumed request token.
    Duplicate,
    /// Apply one representative membership mutation.
    TopologyChange,
    /// Reconstruct durable topology in a fresh process-clock generation.
    Restart,
}

/// Exhaustive environment alphabet for finite retry exploration.
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

/// Environment actions around claimed stabilization reports and their effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StabilizationAction {
    /// Begin a correlated query to the current successor head.
    Begin,
    /// Claim the current report token before connection effects.
    ClaimCurrentProof,
    /// Reserve one candidate permitted by the claimed proof.
    ReserveCurrentCandidate,
    /// Execute one already-reserved connection effect.
    ExecuteReservedCandidate,
    /// Attempt to advance a superseded candidate plan.
    AttemptSupersededCandidate,
    /// Cancel the current report token and candidate plan.
    CancelCurrent,
    /// Commit the current proof when no effect remains reserved.
    CompleteCurrentProof,
    /// Replay a proof retired by cancellation or supersession.
    DeliverSupersededProof,
    /// Deliver the current proof before any handler has claimed it.
    DeliverUnclaimedProof,
    /// Insert a closer successor to invalidate reporter ownership.
    MoveSuccessorHead,
}

/// Exhaustive environment alphabet for stabilization-effect exploration.
const STABILIZATION_ACTIONS: [StabilizationAction; 10] = [
    StabilizationAction::Begin,
    StabilizationAction::ClaimCurrentProof,
    StabilizationAction::ReserveCurrentCandidate,
    StabilizationAction::ExecuteReservedCandidate,
    StabilizationAction::AttemptSupersededCandidate,
    StabilizationAction::CancelCurrent,
    StabilizationAction::CompleteCurrentProof,
    StabilizationAction::DeliverSupersededProof,
    StabilizationAction::DeliverUnclaimedProof,
    StabilizationAction::MoveSuccessorHead,
];

/// Checker state for stabilization token and connection-effect ownership.
///
/// Permit reservation is separate from execution so supersession can interleave
/// between them using only states possible in the production plan.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StabilizationModelState {
    /// Production topology and its internal stabilization token state.
    topology: TopologyState,
    /// Current `(reporter, request_id, claimed)` proof owner, when present.
    current: Option<(Did, uuid::Uuid, bool)>,
    /// Bounded candidate iterator created after the current proof is claimed.
    current_plan: Option<StabilizationConnectionPlan>,
    /// Retired proofs retained only for stale-delivery actions.
    superseded: Vec<(Did, uuid::Uuid)>,
    /// Retired plans retained to verify they cannot emit new effects.
    superseded_plans: Vec<StabilizationConnectionPlan>,
    /// Request IDs with a permitted but not yet executed connection effect.
    reserved_connection_effects: Vec<uuid::Uuid>,
    /// Executed effect counts grouped by the request that authorized them.
    connection_effects: Vec<(uuid::Uuid, u8)>,
    /// Next UUID payload supplied by the stabilization effect boundary.
    next_request_id: u128,
}

impl StabilizationModelState {
    /// Build the initial stabilization model with one known successor.
    ///
    /// No report token, connection plan, reservation, or executed effect is owned,
    /// allowing every lifecycle to begin through the production `BeginStabilize` event.
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

    /// Begin a correlated stabilization query and supersede an unanswered proof.
    ///
    /// Production emits no query while the current proof is claimed, and the
    /// model then keeps that owner. Otherwise retired proofs and plans remain
    /// available only to adversarial replay actions; the current owner is
    /// replaced by the query action emitted from production.
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
        if issued.is_none() && matches!(self.current, Some((_, _, true))) {
            return next;
        }
        // A new query supersedes the older proof but keeps it replayable, so
        // stale delivery is checked instead of assumed impossible.
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

    /// Claim the current report token and create its bounded candidate plan.
    ///
    /// The method is inert without an unclaimed current owner, ensuring that plans
    /// cannot exist before production accepts the reporter and request identity.
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

    /// Commit a claimed report when no reserved connection effect remains pending.
    ///
    /// Completion is blocked between permit reservation and effect execution so a
    /// single report cannot commit topology while one of its side effects is unresolved.
    fn complete(&self, request: Option<(Did, uuid::Uuid, bool)>) -> Self {
        let Some((reporter, request_id, true)) = request else {
            return self.clone();
        };
        // Commit is blocked while a connection permit is reserved; otherwise a
        // single report could both admit peers and prove ranges twice.
        if self.reserved_connection_effects.contains(&request_id) {
            return self.clone();
        }
        self.deliver((reporter, request_id))
    }

    /// Split reservation from execution so churn between permit issue and
    /// connection completion has a concrete interleaving.
    ///
    /// At most one permit per request is outstanding, and candidates come only
    /// from the bounded production connection plan owned by the claimed token.
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

    /// Execute one previously reserved candidate connection effect.
    ///
    /// Popping the reservation before recording execution witnesses that a permit
    /// is single-use even when later actions replay or supersede its source report.
    fn execute_reserved_candidate(&self) -> Self {
        let mut next = self.clone();
        if let Some(request_id) = next.reserved_connection_effects.pop() {
            next.record_connection_effect(request_id);
        }
        next
    }

    /// The environment may still try an old plan after supersession; the
    /// production plan must reject new side effects from it.
    ///
    /// Any unexpected `Connect` step is recorded so the model assertions expose
    /// stale-plan authority rather than silently discarding the violation.
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

    /// Count one executed candidate connection against its authorizing request ID.
    ///
    /// Per-request totals let the search enforce the successor-capacity-plus-one
    /// fan-out bound across all reservation and supersession interleavings.
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

    /// Cancel the current stabilization token and retire all authority derived from it.
    ///
    /// The proof and plan are retained solely as stale replay inputs; neither may
    /// remain current or authorize a future candidate connection.
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

    /// Deliver a stabilization report for either current or superseded token state.
    ///
    /// Production correlation decides whether topology changes; only a matching,
    /// claimed current owner is removed from the model after successful consumption.
    fn deliver(&self, request: (Did, uuid::Uuid)) -> Self {
        let (reporter, request_id) = request;
        let output = step(
            &self.topology,
            TopologyEvent::Stabilize {
                reporter,
                request_id,
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

    /// Interpret one adversarial stabilization action against the model state.
    ///
    /// The pure mapping exposes every claim, reservation, execution, cancellation,
    /// replay, and successor-head interleaving to breadth-first exploration.
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
            StabilizationAction::DeliverUnclaimedProof => match self.current {
                Some((reporter, request_id, false)) => self.deliver((reporter, request_id)),
                _ => self.clone(),
            },
            StabilizationAction::MoveSuccessorHead => {
                // A closer successor invalidates the reporter binding even if
                // the old proof arrives later.
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

/// Prove a stabilization plan has bounded fan-out and loses authority when stale.
///
/// The test exhausts one claimed plan, then supersedes another between reservation
/// and execution to distinguish already-permitted work from forbidden future work.
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
    // A new maintenance round cannot supersede the claimed report; a head
    // change can.
    assert_eq!(reserved.transition(StabilizationAction::Begin), reserved);
    let superseded = reserved.transition(StabilizationAction::MoveSuccessorHead);
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

/// Prove stabilization tokens exclusively gate range proofs and connection effects.
///
/// Finite exploration checks stale reports, stale plans, unique reservations, the
/// per-request effect cap, and proof application only after a valid claimed commit.
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
                // A report whose token is still `Requested` has not been
                // claimed by any handler and must not change topology.
                if matches!(action, StabilizationAction::DeliverUnclaimedProof)
                    && matches!(state.current, Some((_, _, false)))
                {
                    assert_eq!(next.topology, state.topology);
                }
                // A new round never revokes a claimed report; only head
                // movement or cancellation retires it.
                if matches!(action, StabilizationAction::Begin)
                    && matches!(state.current, Some((_, _, true)))
                {
                    assert_eq!(next.current, state.current);
                    assert_eq!(next.current_plan, state.current_plan);
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

/// Prove listener restart preserves the remaining lookup timeout duration.
///
/// Re-basing production status onto a fresh listener clock must yield the same
/// 9.75-second remainder rather than restarting or prematurely expiring the lease.
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

/// Prove finite retry exploration preserves all bounded resource laws.
///
/// Every reachable action schedule checks unique request IDs, per-process emission
/// spacing, exclusive ownership phases, exact deadline cleanup, and backoff growth.
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

/// Prove a delayed pre-restart report cannot match the next production request.
///
/// Reconstruction drops scheduler ownership while retaining the UUID sequence, so
/// the newly emitted token differs and replaying the old one leaves state unchanged.
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
