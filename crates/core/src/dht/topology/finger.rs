//! Finger-specific adapters for the pure topology transition model.
//!
//! [`crate::dht::finger`] owns the convergence algorithm. This module supplies
//! only the surrounding topology data and translates a reserved lookup into a
//! [`TopologyAction`]. Transport admission and message delivery remain outside
//! both layers.
//!
//! # Algorithm flow
//!
//! ```text
//! [advance(now, request_id)]
//!            |
//!            v
//! [successor and finger slots exist?] -- no --> [leave state dormant]
//!            |
//!           yes
//!            v
//! [skip slots proved by successor stabilization]
//!            |
//!            v
//! [reserve next due, unproved remote range after the cursor] -- none --> [persist pacing state]
//!            |
//!            v
//! [find_successor(range target)]
//!       |                       |
//!      local                  remote
//!       |                       |
//!       v                       v
//! [apply_result]       [emit FindSuccessorForFix]
//!                               |
//!                               v
//!                 [authenticated lookup result]
//!                    |       |       |       |
//!                  apply   defer   retire  cancel
//!                    |       |       |       |
//!                    +-------+-------+-------+
//!                               |
//!                               v
//!                  [update convergence metadata]
//! ```

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

/// Reserve and route at most one lookup for the next unverified remote slot.
///
/// Slots covered by the current successor interval are left to stabilization.
/// For the next due slot outside that interval (cyclically after the previous
/// attempt), this transition records the exact request token before resolving
/// the lookup target. A locally resolved
/// target is applied immediately; a remote target emits exactly one
/// [`TopologyAction::FindSuccessorForFix`] carrying the same token. Isolated
/// nodes and states with no due work are returned without network actions.
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
    let Some(request) =
        convergence.prepare_lookup(&state.fingers, first_remote_slot(state), now_ms, request_id)
    else {
        return TopologyStep {
            state: TopologyState {
                finger_convergence: convergence,
                ..state.clone()
            },
            actions: Vec::new(),
        };
    };

    // Finger lookups probe the ring position relative to the local node.
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

/// First slot whose target lies outside the local successor interval.
///
/// Slots at or before that boundary are proved by stabilization, so routed
/// finger lookups and the revalidation gate consider only slots from here on.
fn first_remote_slot(state: &TopologyState) -> usize {
    local_successor_range_end(state)
        .map(|end| end.saturating_add(1))
        .unwrap_or(0)
}

/// Reopen the range after the current cursor without emitting network work.
///
/// The transition invalidates only the convergence proof range selected by
/// [`FingerConvergenceState::begin_revalidation`](crate::dht::finger::FingerConvergenceState::begin_revalidation).
/// It preserves topology hints and delegates all later pacing and lookup
/// reservation to [`advance`].
pub(super) fn begin_revalidation(state: &TopologyState) -> TopologyState {
    let mut convergence = state.finger_convergence.clone();
    convergence.begin_revalidation(
        &state.fingers,
        state.fix_finger_index,
        first_remote_slot(state),
    );
    TopologyState {
        finger_convergence: convergence,
        ..state.clone()
    }
}

/// Apply a proved range and advance the periodic cursor only on real progress.
///
/// The request token, successor, and timestamp are validated by the convergence
/// state before any finger slot changes. An accepted proof rewrites the proved
/// range and moves `fix_finger_index` to its inclusive end; a rejected proof
/// preserves the prior public cursor while retaining any retry metadata emitted
/// by the convergence algorithm.
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
///
/// This transition does not modify finger hints. It validates that the exact
/// lookup is still live and records the proved successor as deferred evidence
/// so transport admission can consume it later. Expired, mismatched, or stale
/// evidence is represented by the returned [`FingerDeferOutcome`].
fn defer_result(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyState, FingerDeferOutcome) {
    let mut convergence = state.finger_convergence.clone();
    let outcome = convergence.defer_result(state.local, &state.fingers, request, successor, now_ms);
    (
        TopologyState {
            finger_convergence: convergence,
            ..state.clone()
        },
        outcome,
    )
}

/// Wrap [`apply_result`] as a pure topology step with no side effects.
///
/// The returned action list is always empty because applying authenticated
/// lookup evidence is a local state transition. The companion outcome exposes
/// whether the request proved a range or was rejected without asking callers to
/// reconstruct that decision from the resulting topology snapshot.
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

/// Wrap [`defer_result`] as a pure topology step with no side effects.
///
/// Deferral only transfers ownership of a live proof to the admission lease, so
/// the returned [`TopologyStep`] never contains a transport action. Callers use
/// the returned outcome to decide whether the candidate connection may continue.
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
///
/// Retirement is accepted only for the request and successor currently owned
/// by the convergence state. The transition removes that candidate evidence and
/// records the algorithm's retry state without changing successor, predecessor,
/// or finger hints; stale retirement attempts therefore cannot cancel newer work.
pub(crate) fn retire_candidate(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> (TopologyStep, FingerRetireOutcome) {
    let mut convergence = state.finger_convergence.clone();
    let outcome =
        convergence.retire_result(state.local, &state.fingers, request, successor, now_ms);
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

/// Cancel an in-flight or deferred finger proof and start its retry backoff.
///
/// Cancellation is correlated by the complete [`FingerFixRequest`], allowing
/// the convergence state to ignore an obsolete send failure. A matching request
/// releases its lookup or admission ownership and schedules the next attempt
/// from `now_ms`; all topology hints remain unchanged.
pub(super) fn cancel(
    state: &TopologyState,
    request: FingerFixRequest,
    now_ms: u64,
) -> TopologyState {
    let mut convergence = state.finger_convergence.clone();
    convergence.cancel(&state.fingers, request, now_ms);
    TopologyState {
        finger_convergence: convergence,
        ..state.clone()
    }
}
