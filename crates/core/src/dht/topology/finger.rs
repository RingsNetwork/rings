//! Finger-specific adapters for the pure topology transition model.
//!
//! [`crate::dht::finger`] owns the convergence algorithm. This module supplies
//! only the surrounding topology data and translates a reserved lookup into a
//! [`TopologyAction`]. Transport admission and message delivery remain outside
//! both layers.

use super::find_successor;
use super::stabilization::local_successor_range_end;
use super::stabilization::successor_head;
use super::FindSuccessorStep;
use super::TopologyAction;
use super::TopologyState;
use super::TopologyStep;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerDeferOutcome;
use crate::dht::finger::FingerRetireOutcome;
use crate::dht::Did;
use crate::dht::FingerFixRequest;

/// Reserve and route at most one lookup for the first unverified remote slot.
pub(super) fn advance(state: &TopologyState, now_ms: u64, request_id: uuid::Uuid) -> TopologyStep {
    // Isolation supplies no evidence that an empty range is globally empty.
    // Keep the range unverified but dormant until a successor is admitted.
    if state.fingers.is_empty() || successor_head(state).is_none() {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }

    let mut convergence = state.finger_convergence.clone();
    let first_remote_slot = local_successor_range_end(state)
        .map(|end| end.saturating_add(1))
        .unwrap_or(0);
    let Some(request) =
        convergence.prepare_lookup(&state.fingers, first_remote_slot, now_ms, request_id)
    else {
        return TopologyStep {
            state: TopologyState {
                finger_convergence: convergence,
                ..state.clone()
            },
            actions: Vec::new(),
        };
    };

    let target = state.local + Did::power_of_two(request.slot_index());
    let prepared = TopologyState {
        finger_convergence: convergence,
        ..state.clone()
    };
    match find_successor(&prepared, target) {
        FindSuccessorStep::Local(successor) => TopologyStep {
            state: apply_result(&prepared, request, successor, now_ms).0,
            actions: Vec::new(),
        },
        FindSuccessorStep::Remote { next, did } => TopologyStep {
            state: prepared,
            actions: vec![TopologyAction::FindSuccessorForFix { next, did, request }],
        },
    }
}

/// Reopen the range after the current cursor without emitting network work.
pub(super) fn begin_revalidation(state: &TopologyState) -> TopologyState {
    let mut convergence = state.finger_convergence.clone();
    convergence.begin_revalidation(&state.fingers, state.fix_finger_index);
    TopologyState {
        finger_convergence: convergence,
        ..state.clone()
    }
}

/// Apply a proved range and advance the periodic cursor only on real progress.
pub(super) fn apply_result(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyState, FingerApplyOutcome) {
    let mut fingers = state.fingers.clone();
    let mut convergence = state.finger_convergence.clone();
    let outcome = convergence.apply_result(state.local, &mut fingers, request, successor, now_ms);
    let fix_finger_index = match outcome {
        FingerApplyOutcome::Applied { end } => end,
        FingerApplyOutcome::Rejected(_) => state.fix_finger_index,
    };
    (
        TopologyState {
            fingers,
            fix_finger_index,
            finger_convergence: convergence,
            ..state.clone()
        },
        outcome,
    )
}

/// Transfer a timely proof from lookup ownership to the admission lease.
fn defer_result(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyState, FingerDeferOutcome) {
    let mut convergence = state.finger_convergence.clone();
    let outcome =
        convergence.defer_result(state.local, state.fingers.len(), request, successor, now_ms);
    (
        TopologyState {
            finger_convergence: convergence,
            ..state.clone()
        },
        outcome,
    )
}

pub(crate) fn apply(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyStep, FingerApplyOutcome) {
    let (state, outcome) = apply_result(state, request, successor, now_ms);
    (
        TopologyStep {
            state,
            actions: Vec::new(),
        },
        outcome,
    )
}

pub(crate) fn defer(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyStep, FingerDeferOutcome) {
    let (state, outcome) = defer_result(state, request, successor, now_ms);
    (
        TopologyStep {
            state,
            actions: Vec::new(),
        },
        outcome,
    )
}

/// Validate and retire an exact candidate that cannot become routable.
pub(crate) fn retire_candidate(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyStep, FingerRetireOutcome) {
    let mut convergence = state.finger_convergence.clone();
    let outcome =
        convergence.retire_result(state.local, state.fingers.len(), request, successor, now_ms);
    (
        TopologyStep {
            state: TopologyState {
                finger_convergence: convergence,
                ..state.clone()
            },
            actions: Vec::new(),
        },
        outcome,
    )
}

pub(super) fn cancel(
    state: &TopologyState,
    request: FingerFixRequest,
    now_ms: u64,
) -> TopologyState {
    let mut convergence = state.finger_convergence.clone();
    convergence.cancel(request, now_ms);
    TopologyState {
        finger_convergence: convergence,
        ..state.clone()
    }
}
