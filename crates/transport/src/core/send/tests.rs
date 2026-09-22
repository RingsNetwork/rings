//! Finite-state exploration of the production reducer composed with its boundary protocol.
//! Two failure observers represent caller and worker; repeat commands are coalesced.
//! All reachable states are explored to a fixed point, not random Tokio schedules.

use std::collections::HashMap;
use std::collections::VecDeque;

use super::model::close_step;
use super::model::failure_effect;
use super::model::CloseEffect;
use super::model::CloseEvent;
use super::model::CloseOutcome;
use super::model::CloseState;
use super::model::FailureEffect;
use crate::core::admission::AdmissionEvent;
use crate::core::admission::AdmissionPhase;

/// Generation gate phases, including the first-poll lock interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Gate {
    Open,
    Admitting,
    Polling,
    Retired,
}
/// Actual send resource owner; caller cancellation after handoff does not release worker IO.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Resource {
    Inline,
    Detached,
    Released,
}
/// Microsteps of a failure observer, exposing snapshot/fence/enqueue/drop race windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Observer {
    Absent,
    Watching,
    Ignored,
    Observed,
    Fenced,
    Submitted,
    Released,
}
/// Physical close can outlive the actor's timeout result; success remains independent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Physical {
    Unstarted,
    Pending,
    Succeeded,
    Failed,
}
/// Targeted mutations demonstrate that the invariants reject the relevant bugs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mutation {
    Faithful,
    SkipFence,
    DuplicateClose,
    ShutdownSuccess,
    EarlyErrorReturn,
}

/// Composition state; each field corresponds to an actual ownership boundary or observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct Model {
    /// Shared atomic permit phase, using its production transition function.
    admission: AdmissionPhase,
    /// Cross-channel first-poll/fence serialization.
    gate: Gate,
    /// Unique primitive resource location.
    resource: Resource,
    /// Caller and detached worker's failure-delivery progress.
    observers: [Observer; 2],
    /// Actor-local state, advanced by the exact production close_step.
    actor: CloseState,
    /// Capacity-one retirement mailbox occupancy.
    mailbox: bool,
    /// Whether the executor may schedule actor/worker IO.
    running: bool,
    /// History witness for at-most-once physical-close initiation.
    starts: u8,
    /// Independent underlying physical operation and success witness.
    physical: Physical,
    /// History bit detecting a failed observer released before synchronous fencing.
    unsafe_release: bool,
    /// An error response observed by the caller, distinct from resource destruction.
    error_returned: bool,
}

impl Model {
    /// Initial state before permit claim, first polling, or retirement.
    fn initial() -> Self {
        Self {
            admission: AdmissionPhase::Pending,
            gate: Gate::Open,
            resource: Resource::Inline,
            observers: [Observer::Watching, Observer::Absent],
            actor: CloseState::Idle,
            mailbox: false,
            running: true,
            starts: 0,
            physical: Physical::Unstarted,
            unsafe_release: false,
            error_returned: false,
        }
    }

    /// Preserve all unselected observers while advancing one boundary participant.
    fn observer(self, index: usize, phase: Observer) -> Self {
        Self {
            observers: std::array::from_fn(|i| if i == index { phase } else { self.observers[i] }),
            ..self
        }
    }

    /// Safety invariants are independent assertions over composed state.
    fn lawful(self) -> bool {
        let started_safely = self.starts == 0 || self.gate == Gate::Retired;
        let command_is_fenced = !self.mailbox || self.gate == Gate::Retired;
        let physical_is_authorized = self.physical == Physical::Unstarted || self.starts == 1;
        let actor_is_sound = match self.actor {
            CloseState::Idle | CloseState::Finished(CloseOutcome::Interrupted) => true,
            CloseState::Closing | CloseState::Finished(CloseOutcome::Failed) => self.starts == 1,
            CloseState::Finished(CloseOutcome::Succeeded) => {
                self.starts == 1 && self.physical == Physical::Succeeded
            }
            CloseState::Finished(CloseOutcome::Unused) => self.starts == 0,
        };
        let error_waited =
            !self.error_returned || self.gate != Gate::Retired || self.actor.outcome().is_some();
        error_waited
            && !self.unsafe_release
            && self.starts <= 1
            && started_safely
            && command_is_fenced
            && physical_is_authorized
            && actor_is_sound
    }

    /// Execute one actor event through the production pure reducer.
    fn actor_event(self, event: CloseEvent, mutation: Mutation) -> Self {
        let (actor, effect) = match (mutation, event, self.actor) {
            (Mutation::ShutdownSuccess, CloseEvent::RuntimeStopped, _) => (
                CloseState::Finished(CloseOutcome::Succeeded),
                CloseEffect::Publish(CloseOutcome::Succeeded),
            ),
            (Mutation::DuplicateClose, CloseEvent::Fenced, CloseState::Closing) => {
                (CloseState::Closing, CloseEffect::PublishClosingAndStart)
            }
            _ => close_step(self.actor, event),
        };
        Self {
            actor,
            starts: self.starts + u8::from(effect == CloseEffect::PublishClosingAndStart),
            ..self
        }
    }
}

/// Inline first poll is indivisible with respect to fencing, but completion may race later observers.
fn send_edges(s: Model) -> Vec<(&'static str, Model)> {
    let claim = s.running
        && s.gate == Gate::Open
        && s.admission == AdmissionPhase::Pending
        && s.resource == Resource::Inline
        && s.observers[0] == Observer::Watching;
    let first_poll = s.gate == Gate::Polling;
    let admitted = s.gate == Gate::Admitting && s.admission == AdmissionPhase::Pending;
    let rejected = s.gate == Gate::Admitting;
    let cancel = s.admission == AdmissionPhase::Pending;
    let accepted = s.running
        && s.resource == Resource::Detached
        && s.admission == AdmissionPhase::Irrevocable
        && s.observers[1] == Observer::Watching;
    [
        claim.then_some({
            ("acquire admission gate", Model {
                gate: Gate::Admitting,
                ..s
            })
        }),
        admitted.then(|| {
            ("claim permit", Model {
                gate: Gate::Polling,
                admission: s
                    .admission
                    .transition(AdmissionEvent::MarkIrrevocable)
                    .expect("legal claim"),
                ..s
            })
        }),
        rejected.then_some({
            ("permit rejected or pre-claim panic / unlock", Model {
                gate: Gate::Open,
                observers: [Observer::Ignored, Observer::Absent],
                ..s
            })
        }),
        first_poll.then_some({
            ("first poll pending / handoff", Model {
                gate: Gate::Open,
                resource: Resource::Detached,
                observers: [Observer::Watching, Observer::Watching],
                ..s
            })
        }),
        first_poll.then_some({
            ("first poll error or panic / unlock", Model {
                gate: Gate::Open,
                observers: [Observer::Observed, Observer::Absent],
                ..s
            })
        }),
        first_poll.then(|| {
            ("first poll accepted / unlock", Model {
                gate: Gate::Open,
                admission: s
                    .admission
                    .transition(AdmissionEvent::Accept)
                    .expect("legal acceptance"),
                resource: Resource::Released,
                observers: [Observer::Released, Observer::Absent],
                ..s
            })
        }),
        cancel.then(|| {
            ("cancel before claim", Model {
                admission: s
                    .admission
                    .transition(AdmissionEvent::Cancel)
                    .expect("legal cancellation"),
                ..s
            })
        }),
        accepted.then(|| {
            ("detached acceptance", Model {
                admission: s
                    .admission
                    .transition(AdmissionEvent::Accept)
                    .expect("legal acceptance"),
                resource: Resource::Released,
                ..s.observer(1, Observer::Released)
            })
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Expand failure delivery into distinct microsteps; every submitted command carries fencing evidence.
fn observer_edges(s: Model, mutation: Mutation) -> Vec<(&'static str, Model)> {
    s.observers
        .into_iter()
        .enumerate()
        .filter_map(|(index, phase)| match phase {
            Observer::Watching if !matches!(s.gate, Gate::Admitting | Gate::Polling) => {
                let next = match failure_effect(s.admission) {
                    FailureEffect::Ignore => Observer::Ignored,
                    FailureEffect::FenceThenNotify => Observer::Observed,
                };
                Some(("observe failure snapshot", s.observer(index, next)))
            }
            Observer::Observed if !matches!(s.gate, Gate::Admitting | Gate::Polling) => {
                Some(("commit synchronous fence", Model {
                    gate: match mutation {
                        Mutation::SkipFence => s.gate,
                        _ => Gate::Retired,
                    },
                    ..s.observer(index, Observer::Fenced)
                }))
            }
            Observer::Fenced => Some(("submit / coalesce fenced command", Model {
                mailbox: s.running
                    && (s.actor == CloseState::Idle
                        || (mutation == Mutation::DuplicateClose
                            && s.actor == CloseState::Closing)),
                ..s.observer(index, Observer::Submitted)
            })),
            Observer::Ignored | Observer::Submitted => {
                Some(("release observer resources", Model {
                    unsafe_release: s.unsafe_release
                        || (phase == Observer::Submitted && s.gate != Gate::Retired),
                    resource: match (index, s.resource) {
                        (0, Resource::Inline) | (1, Resource::Detached) => Resource::Released,
                        _ => s.resource,
                    },
                    ..s.observer(index, Observer::Released)
                }))
            }
            _ => None,
        })
        .collect()
}

/// Actor and underlying physical IO are separate, including timeout followed by late physical success.
fn actor_edges(s: Model, mutation: Mutation) -> Vec<(&'static str, Model)> {
    let receive = s.running && s.mailbox;
    let no_observers = s
        .observers
        .iter()
        .all(|o| matches!(o, Observer::Absent | Observer::Released));
    let unused = s.running && !s.mailbox && no_observers && s.actor == CloseState::Idle;
    let closing = s.running && s.actor == CloseState::Closing;
    let io_start = closing && s.physical == Physical::Unstarted;
    let io_pending = s.running && s.physical == Physical::Pending;
    [
        receive.then(|| {
            ("actor receives fenced command", Model {
                mailbox: false,
                ..s.actor_event(CloseEvent::Fenced, mutation)
            })
        }),
        unused.then(|| {
            (
                "all observers gone",
                s.actor_event(CloseEvent::ObserversGone, mutation),
            )
        }),
        io_start.then_some({
            ("physical close starts polling", Model {
                physical: Physical::Pending,
                ..s
            })
        }),
        io_pending.then_some({
            ("physical close succeeds", Model {
                physical: Physical::Succeeded,
                ..s
            })
        }),
        io_pending.then_some({
            ("physical close fails", Model {
                physical: Physical::Failed,
                ..s
            })
        }),
        (closing && s.physical == Physical::Succeeded).then(|| {
            (
                "actor publishes success",
                s.actor_event(CloseEvent::CloseSucceeded, mutation),
            )
        }),
        closing.then(|| {
            (
                "actor publishes close error or timeout",
                s.actor_event(CloseEvent::CloseFailed, mutation),
            )
        }),
        s.running.then(|| {
            ("executor shutdown", Model {
                running: false,
                mailbox: false,
                ..s.actor_event(CloseEvent::RuntimeStopped, mutation)
            })
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Error return is distinct from dropping a failed observer's resources.
fn return_edges(s: Model, mutation: Mutation) -> Vec<(&'static str, Model)> {
    let caller_finished = s.observers[0] == Observer::Released
        && s.admission != AdmissionPhase::Accepted
        && !s.error_returned;
    let cleanup_observed = s.gate != Gate::Retired || s.actor.outcome().is_some();
    (caller_finished && (cleanup_observed || mutation == Mutation::EarlyErrorReturn))
        .then_some(("return send error", Model {
            error_returned: true,
            ..s
        }))
        .into_iter()
        .collect()
}

/// Reconstruct a shortest BFS counterexample rather than reporting only a failed assertion.
fn trace(
    mut state: Model,
    parents: &HashMap<Model, Option<(Model, &'static str)>>,
) -> Vec<&'static str> {
    let mut actions = Vec::new();
    while let Some(Some((previous, action))) = parents.get(&state) {
        actions.push(*action);
        state = *previous;
    }
    actions.reverse();
    actions
}

/// Explore the finite graph to a fixed point; no arbitrary depth cut-off hides a state.
fn explore(mutation: Mutation) -> Result<usize, Vec<&'static str>> {
    let initial = Model::initial();
    let mut queue = VecDeque::from([initial]);
    let mut parents = HashMap::from([(initial, None)]);
    while let Some(state) = queue.pop_front() {
        for (action, next) in send_edges(state)
            .into_iter()
            .chain(observer_edges(state, mutation))
            .chain(actor_edges(state, mutation))
            .chain(return_edges(state, mutation))
        {
            if !next.lawful() {
                let mut counterexample = trace(state, &parents);
                counterexample.push(action);
                return Err(counterexample);
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = parents.entry(next) {
                entry.insert(Some((state, action)));
                queue.push_back(next);
            }
        }
    }
    if mutation == Mutation::Faithful {
        // Non-vacuity names semantic race witnesses rather than an arbitrary state-count threshold.
        assert!(parents
            .keys()
            .any(|s| s.observers == [Observer::Observed, Observer::Observed]));
        assert!(parents.keys().any(
            |s| s.admission == AdmissionPhase::Accepted && s.observers[0] == Observer::Observed
        ));
        assert!(parents
            .keys()
            .any(|s| s.actor == CloseState::Finished(CloseOutcome::Failed)
                && s.physical == Physical::Succeeded));
        assert!(parents
            .keys()
            .any(|s| s.gate == Gate::Admitting && s.admission == AdmissionPhase::Cancelled));
        assert!(parents
            .keys()
            .any(|s| !s.running && s.gate == Gate::Polling));
    }
    Ok(parents.len())
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn all_reachable_composed_ownership_states_preserve_safety() {
    let count =
        explore(Mutation::Faithful).expect("faithful reducer and boundaries preserve the laws");
    println!("explored {count} reachable ownership states to a fixed point");
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn mutations_expose_fence_close_shutdown_and_error_return_violations() {
    for mutation in [
        Mutation::SkipFence,
        Mutation::DuplicateClose,
        Mutation::ShutdownSuccess,
        Mutation::EarlyErrorReturn,
    ] {
        let counterexample = explore(mutation).expect_err("mutated shell must violate a law");
        println!("{mutation:?}: {counterexample:?}");
        assert!(!counterexample.is_empty());
    }
}

/// Every pair of poll/destruction observations reports at most one terminal failure.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn observation_algebra_is_total_and_terminal_states_are_absorbing() {
    use super::model::observation_step;
    use super::model::Observation;
    use super::model::ObservationEffect;
    use super::model::ObservationState;

    let observations = [
        Observation::Pending,
        Observation::Succeeded,
        Observation::Failed,
        Observation::Panicked,
        Observation::Abandoned,
    ];
    for first in observations {
        let (state, effect) = observation_step(ObservationState::Active, first);
        let failure = matches!(
            first,
            Observation::Failed | Observation::Panicked | Observation::Abandoned
        );
        assert_eq!(effect == ObservationEffect::ReportFailure, failure);
        assert_eq!(
            state == ObservationState::Active,
            first == Observation::Pending
        );
        for second in observations {
            let (next, next_effect) = observation_step(state, second);
            let reports = usize::from(effect == ObservationEffect::ReportFailure)
                + usize::from(next_effect == ObservationEffect::ReportFailure);
            assert!(reports <= 1);
            if state == ObservationState::Finished {
                assert_eq!(next, state);
                assert_eq!(next_effect, ObservationEffect::None);
            }
        }
    }
}
