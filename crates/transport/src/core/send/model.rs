//! Pure transition algebra for send ownership and the one-shot close actor.
//!
//! No locks, tasks, clocks, polling, atomics, or IO belong here. Adapters interpret
//! effects; model exploration uses these same functions, not a second algorithm.

use crate::core::admission::AdmissionPhase;

/// Immutable failure decision, derived from one atomic admission observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum FailureEffect {
    /// No uncertain physical write was observed.
    Ignore,
    /// Synchronously fence before delivering the actor's coalescible command.
    FenceThenNotify,
}

/// Pre: `admission` is one coherent snapshot of the shared permit machine.
/// Post: FenceThenNotify iff that snapshot is Irrevocable, never Accepted.
/// The decision remains valid if acceptance subsequently races with fencing.
pub(crate) const fn failure_effect(admission: AdmissionPhase) -> FailureEffect {
    match admission {
        AdmissionPhase::Irrevocable => FailureEffect::FenceThenNotify,
        AdmissionPhase::Pending | AdmissionPhase::Cancelled | AdmissionPhase::Accepted => {
            FailureEffect::Ignore
        }
    }
}

/// Terminal result of the actor, independent of the backend's physical-close witness.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum CloseOutcome {
    /// All observation rights disappeared without requesting retirement.
    Unused,
    /// The injected physical-close operation returned success.
    Succeeded,
    /// The close operation returned a typed error, including its timeout.
    Failed,
    /// Executor cancellation or panic interrupted the actor; no success inferred.
    Interrupted,
}

/// Actor-local lifecycle state. Copy duplicates a mathematical value, not authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum CloseState {
    /// The actor exclusively owns the unpolled close capability.
    Idle,
    /// A fenced command consumed the unique right to start close.
    Closing,
    /// Terminal states are absorbing, including against later executor shutdown.
    Finished(CloseOutcome),
}

impl CloseState {
    /// Project only terminal actor outcomes, without equating them with physical closure.
    pub(crate) const fn outcome(self) -> Option<CloseOutcome> {
        match self {
            Self::Finished(outcome) => Some(outcome),
            Self::Idle | Self::Closing => None,
        }
    }
}

/// Inputs delivered by the mailbox, close adapter, or actor destruction boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum CloseEvent {
    /// The generation gate was synchronously fenced before enqueueing this command.
    Fenced,
    /// The mailbox is empty and every observer has released its sender.
    ObserversGone,
    /// The physical-close future returned Ok.
    CloseSucceeded,
    /// The physical-close future returned Err.
    CloseFailed,
    /// The actor was destroyed before completing its protocol.
    RuntimeStopped,
}

/// Effects interpreted by the actor shell after committing the returned state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloseEffect {
    /// Duplicate or inapplicable event; preserve state and ownership.
    None,
    /// Publish Closing, then initiate the single close operation; polling may repeat.
    PublishClosingAndStart,
    /// Publish an explicit terminal result to watchers.
    Publish(CloseOutcome),
}

/// Pure State × Event -> State × Effect; no hidden reads or mutation.
///
/// Invariant: PublishClosingAndStart occurs only on Idle -> Closing. Finished is absorbing.
/// Preservation: duplicates stutter; only Closing can publish close success/failure.
/// RuntimeStopped publishes Interrupted and cannot manufacture physical success.
pub(crate) const fn close_step(state: CloseState, event: CloseEvent) -> (CloseState, CloseEffect) {
    use CloseEffect::None;
    use CloseEffect::Publish;
    use CloseEffect::PublishClosingAndStart;
    use CloseEvent::CloseFailed;
    use CloseEvent::CloseSucceeded;
    use CloseEvent::Fenced;
    use CloseEvent::ObserversGone;
    use CloseEvent::RuntimeStopped;
    use CloseOutcome::Failed;
    use CloseOutcome::Interrupted;
    use CloseOutcome::Succeeded;
    use CloseOutcome::Unused;
    use CloseState::Closing;
    use CloseState::Finished;
    use CloseState::Idle;
    match (state, event) {
        (Idle, Fenced) => (Closing, PublishClosingAndStart),
        (Idle, ObserversGone) => (Finished(Unused), Publish(Unused)),
        (Closing, CloseSucceeded) => (Finished(Succeeded), Publish(Succeeded)),
        (Closing, CloseFailed) => (Finished(Failed), Publish(Failed)),
        (Idle | Closing, RuntimeStopped) => (Finished(Interrupted), Publish(Interrupted)),
        _ => (state, None),
    }
}

/// Local observation state; it carries no IO ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationState {
    /// Polling, failure, or abandonment can still affect the send.
    Active,
    /// A terminal poll result was already observed.
    Finished,
}

/// Values reported by the polling/destruction adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    /// The primitive remains pending.
    Pending,
    /// A terminal successful result was returned.
    Succeeded,
    /// A terminal failed result was returned.
    Failed,
    /// Polling unwound; report before resuming that unwind.
    Panicked,
    /// The observer is being destroyed without a result.
    Abandoned,
}

/// Effects interpreted at the synchronous resource-owner boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationEffect {
    /// No failure needs to be reported.
    None,
    /// Read the permit snapshot and execute failure_effect before resource release.
    ReportFailure,
}

/// Post: each observer reports at most one terminal failure; duplicates stutter.
pub(crate) const fn observation_step(
    state: ObservationState,
    event: Observation,
) -> (ObservationState, ObservationEffect) {
    use Observation::Abandoned;
    use Observation::Failed;
    use Observation::Panicked;
    use Observation::Pending;
    use Observation::Succeeded;
    use ObservationEffect::None;
    use ObservationEffect::ReportFailure;
    use ObservationState::Active;
    use ObservationState::Finished;
    match (state, event) {
        (Active, Pending) => (Active, None),
        (Active, Succeeded) => (Finished, None),
        (Active, Failed | Panicked | Abandoned) => (Finished, ReportFailure),
        (Finished, _) => (Finished, None),
    }
}

/// Pure checked offset arithmetic; the adapter owns the serialized counter access.
pub(crate) const fn end_offset(current: u64, bytes: u64) -> Option<u64> {
    current.checked_add(bytes)
}
