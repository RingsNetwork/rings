//! Pure HMCC/Zave predecessor and successor stabilization propositions.

use super::dist;
use super::successors;
use super::Did;
use super::StabilizationPhase;
use super::StabilizationRequest;
use super::TopologyAction;
use super::TopologyState;
use super::TopologyStep;
use crate::dht::finger::finger_proof_end;

/// Bounded connection-effect cursor for one claimed stabilization report.
///
/// The handler and formal model both consume this production transition. It
/// revalidates the claimed token before every candidate and owns the hard
/// per-report effect bound.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct StabilizationConnectionPlan {
    /// Successor that produced the claimed topology report.
    reporter: Did,
    /// Correlation token echoed by that report.
    request_id: uuid::Uuid,
    /// Bounded, deduplicated peers reported by the successor.
    candidates: Vec<Did>,
    /// Cursor for the next candidate whose connection effect may run.
    next_candidate: usize,
}

/// Next permitted effect for a stabilization connection plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StabilizationConnectionStep {
    /// Connect this bounded candidate, then re-enter the transition.
    Connect {
        /// Candidate admitted by this step.
        candidate: Did,
        /// Claimed report whose budget owns the effect.
        request_id: uuid::Uuid,
    },
    /// Every candidate was consumed while the claim remained current.
    Complete,
    /// The report was superseded; no further network effect is permitted.
    Stale,
}

impl StabilizationConnectionPlan {
    /// Create a bounded candidate cursor for one claimed stabilization report.
    pub(crate) fn new(
        reporter: Did,
        request_id: uuid::Uuid,
        candidates: impl IntoIterator<Item = Did>,
        local: Did,
        successor_capacity: usize,
    ) -> Self {
        // The predecessor is allowed one slot in addition to the local
        // successor-list capacity.
        let capacity = successor_capacity.saturating_add(1);
        let mut bounded = Vec::with_capacity(capacity);
        for candidate in candidates {
            if bounded.len() == capacity {
                break;
            }
            if candidate != local && !bounded.contains(&candidate) {
                bounded.push(candidate);
            }
        }
        Self {
            reporter,
            request_id,
            candidates: bounded,
            next_candidate: 0,
        }
    }

    /// Return the next candidate only while the report claim is still current.
    pub(crate) fn advance(&mut self, state: &TopologyState) -> StabilizationConnectionStep {
        if !state.is_processing_stabilization_report(self.reporter, self.request_id) {
            return StabilizationConnectionStep::Stale;
        }
        let Some(candidate) = self.candidates.get(self.next_candidate).copied() else {
            return StabilizationConnectionStep::Complete;
        };
        self.next_candidate = self.next_candidate.saturating_add(1);
        StabilizationConnectionStep::Connect {
            candidate,
            request_id: self.request_id,
        }
    }
}

/// `head(s)`: the nearest successor other than `local`, the upper end of the
/// local placement interval `(local, head]`; `None` when the node stands alone.
pub fn successor_head(state: &TopologyState) -> Option<Did> {
    state
        .successors
        .iter()
        .copied()
        .find(|successor| *successor != state.local)
}

/// Highest slot whose target lies in the local successor interval
/// `(local, head]`. That interval is verified by a stabilization report from
/// `head` whose predecessor is `local`, not by routing a query around the ring.
pub(super) fn local_successor_range_end(state: &TopologyState) -> Option<usize> {
    finger_proof_end(state.local, successor_head(state)?, 0, state.fingers.len())
}

/// Correct predecessor value after one HMCC/Zave rectify transition.
///
/// Law: `local` is never its own predecessor, so a candidate equal to `local`
/// leaves the current value; the responsibility interval `(pred, local]` is
/// then never empty by a self-reference.
pub fn rectify_predecessor(local: Did, current: Option<Did>, candidate: Did) -> Option<Did> {
    if candidate == local {
        return current;
    }
    match current {
        Some(cur) if dist(local, cur) >= dist(local, candidate) => Some(cur),
        _ => Some(candidate),
    }
}

/// Correct successor list after one HMCC/Zave stabilize transition.
pub fn stabilize_successors(
    local: Did,
    current: &[Did],
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> Vec<Did> {
    let mut known = vec![local];
    for candidate in current.iter().copied().chain(topo_predecessor).chain(
        topo_successors
            .iter()
            .copied()
            .take(topo_successors.len().saturating_sub(1)),
    ) {
        if !known.contains(&candidate) {
            known.push(candidate);
        }
    }
    successors(&known, local, capacity)
}

/// Improved-successor query emitted by one HMCC/Zave stabilize transition.
pub fn stabilize_query(local: Did, current: &[Did], topo_predecessor: Option<Did>) -> Option<Did> {
    let pred = topo_predecessor?;
    if pred == local {
        return None;
    }
    let old_head = current.iter().copied().min_by_key(|&did| dist(local, did));
    match old_head {
        Some(head) if dist(local, pred) >= dist(local, head) => None,
        _ => Some(pred),
    }
}

/// Notify action emitted after one HMCC/Zave stabilize transition.
pub fn stabilize_notify(local: Did, next_successors: &[Did]) -> Option<Did> {
    next_successors.first().copied().filter(|&did| did != local)
}

/// Return the local finger range proved by an authenticated current-head report.
///
/// `pred(head) = local` proves that `head = succ(local + 1)`. Requiring the
/// reporter to be both the old and retained head prevents a delayed report from
/// proving a range after the local successor has changed.
pub(super) fn stabilized_successor_proof_end(
    state: &TopologyState,
    reporter: Did,
    next_successors: &[Did],
    reported_predecessor: Option<Did>,
) -> Option<usize> {
    if successor_head(state) != Some(reporter)
        || next_successors.first().copied() != Some(reporter)
        || reported_predecessor != Some(state.local)
    {
        return None;
    }
    finger_proof_end(state.local, reporter, 0, state.fingers.len())
}

/// Start a stabilization query against the current successor head.
pub(super) fn step_begin(state: &TopologyState, request_id: uuid::Uuid) -> TopologyStep {
    let Some(reporter) = successor_head(state) else {
        return TopologyStep {
            state: TopologyState {
                pending_stabilization: None,
                ..state.clone()
            },
            actions: Vec::new(),
        };
    };
    TopologyStep {
        state: TopologyState {
            pending_stabilization: Some(StabilizationRequest {
                reporter,
                request_id,
                phase: StabilizationPhase::Requested,
            }),
            ..state.clone()
        },
        actions: vec![TopologyAction::QuerySuccessorTopology {
            successor: reporter,
            request_id,
        }],
    }
}

/// Move a matching stabilization report from requested to processing.
pub(super) fn step_claim(
    state: &TopologyState,
    reporter: Did,
    request_id: uuid::Uuid,
) -> TopologyStep {
    TopologyStep {
        state: TopologyState {
            pending_stabilization: state
                .can_claim_stabilization_report(reporter, request_id)
                .then_some(StabilizationRequest {
                    reporter,
                    request_id,
                    phase: StabilizationPhase::Processing,
                })
                .or(state.pending_stabilization),
            ..state.clone()
        },
        actions: Vec::new(),
    }
}

/// Apply a successor topology report after optional token correlation.
pub(super) fn step_stabilize(
    state: &TopologyState,
    reporter: Did,
    request_id: Option<uuid::Uuid>,
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> TopologyStep {
    // Only exact claimed reports may verify local finger ranges; the public
    // compatibility path (`request_id == None`) may still refine successors.
    let correlated = request_id
        .is_some_and(|request_id| state.is_processing_stabilization_report(reporter, request_id));
    if request_id.is_some() && !correlated {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    let next_successors = stabilize_successors(
        state.local,
        &state.successors,
        topo_successors,
        topo_predecessor,
        capacity,
    );
    let mut actions = Vec::new();
    if let Some(query) = stabilize_query(state.local, &state.successors, topo_predecessor) {
        actions.push(TopologyAction::QuerySuccessorList(query));
    }
    if let Some(notify) = stabilize_notify(state.local, &next_successors) {
        actions.push(TopologyAction::Notify(notify));
    }
    let mut fingers = next_successors
        .iter()
        .copied()
        .fold(state.fingers.clone(), |fingers, successor| {
            super::finger_join(state.local, &fingers, successor)
        });
    // A correlated head report with `pred(head) == local` proves every finger
    // slot whose target lies inside the local successor range.
    let successor_proof_end = correlated
        .then(|| {
            stabilized_successor_proof_end(state, reporter, &next_successors, topo_predecessor)
        })
        .flatten();
    if let Some(end) = successor_proof_end {
        fingers
            .iter_mut()
            .take(end.saturating_add(1))
            .for_each(|finger| *finger = Some(reporter));
    }
    let mut finger_convergence = state.finger_convergence.clone();
    finger_convergence.invalidate_hint_changes(&state.fingers, &fingers);
    // Confirmation only advances the public cursor when it proves a range that
    // was not already current.
    let local_range_progressed =
        successor_proof_end.is_some_and(|end| finger_convergence.confirm_range(0, end));
    let fix_finger_index = if local_range_progressed {
        successor_proof_end.unwrap_or(state.fix_finger_index)
    } else {
        state.fix_finger_index
    };
    TopologyStep {
        state: TopologyState {
            successors: next_successors,
            fingers,
            fix_finger_index,
            finger_convergence,
            pending_stabilization: if correlated {
                None
            } else {
                state.pending_stabilization
            },
            ..state.clone()
        },
        actions,
    }
}
