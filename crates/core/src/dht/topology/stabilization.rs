//! Pure HMCC/Zave predecessor and successor stabilization propositions.
//!
//! # Algorithm flow
//!
//! ```text
//! [BeginStabilize(request_id)]
//!             |
//!             v
//! [current successor head?] -- no --> [clear pending request]
//!             |
//!            yes
//!             v
//! [head's report being processed?] -- yes --> [keep it, emit nothing]
//!             |
//!            no
//!             v
//! [store Requested(reporter, request_id)]
//!             |
//!             v
//! [emit QuerySuccessorTopology]
//!             |
//!             v
//! [authenticated report arrives]
//!             |
//!             v
//! [exact Requested token?] -- no --> [ignore as stale]
//!             |
//!            yes
//!             v
//! [mark Processing and bound candidate list]
//!             |
//!             v
//! [recheck claim before each connection effect]
//!             |
//!             v
//! [merge successors -> query improvement -> notify head]
//!             |
//!             v
//! [reporter stayed head and pred(head) == local?]
//!             |                         |
//!            yes                        no
//!             |                         |
//!             v                         v
//! [prove local finger range]       [retain old proof]
//!             |                         |
//!             +------------+------------+
//!                          |
//!                          v
//!                 [retire correlated token]
//! ```

use super::bounded_connection_candidates;
use super::dist;
use super::push_unique;
use super::rehint;
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
    ///
    /// Every call to [`Self::advance`] rechecks that this DID remains the owner
    /// of `request_id`; a successor-head change therefore makes the plan stale.
    reporter: Did,
    /// Correlation token echoed by the authenticated topology report.
    ///
    /// The token distinguishes this plan from older and newer stabilization
    /// rounds that queried the same reporter.
    request_id: uuid::Uuid,
    /// Bounded, deduplicated peers reported by the successor.
    ///
    /// Construction removes `local`, preserves the order given, and caps the
    /// list at one predecessor candidate plus the successor-list capacity.
    candidates: Vec<Did>,
    /// Cursor for the next candidate whose connection effect may run.
    ///
    /// The cursor advances only after the current claim is revalidated and a
    /// candidate is returned, so each bounded candidate is emitted at most once.
    next_candidate: usize,
}

/// Next permitted effect for a stabilization connection plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StabilizationConnectionStep {
    /// Connect this bounded candidate, then re-enter the transition.
    Connect {
        /// Candidate admitted by this step.
        ///
        /// The caller may perform one connection effect for this DID before
        /// re-entering [`StabilizationConnectionPlan::advance`].
        candidate: Did,
        /// Claimed report whose budget owns the effect.
        ///
        /// Effect handlers propagate this token so a later state transition can
        /// reject the connection if the stabilization round was superseded.
        request_id: uuid::Uuid,
    },
    /// Every candidate was consumed while the claim remained current.
    Complete,
    /// The report was superseded; no further network effect is permitted.
    Stale,
}

impl StabilizationConnectionPlan {
    /// Create a bounded candidate cursor for one claimed stabilization report.
    ///
    /// Candidates are consumed in the order given (the handler orders them by
    /// transport quality) after removing the local DID and duplicates. The
    /// stored list is capped at `successor_capacity + 1`, which gives the
    /// reported predecessor one possible slot without permitting an unbounded
    /// number of connection effects from one report.
    pub(crate) fn new(
        reporter: Did,
        request_id: uuid::Uuid,
        candidates: impl IntoIterator<Item = Did>,
        local: Did,
        successor_capacity: usize,
    ) -> Self {
        Self {
            reporter,
            request_id,
            candidates: bounded_connection_candidates(
                local,
                Self::candidate_capacity(successor_capacity),
                candidates,
            ),
            next_candidate: 0,
        }
    }

    /// The connection budget of one stabilization report: the successor-list
    /// capacity plus one slot for the reported predecessor.
    pub(crate) const fn candidate_capacity(successor_capacity: usize) -> usize {
        successor_capacity.saturating_add(1)
    }

    /// Return the next candidate only while the report claim is still current.
    ///
    /// A superseded reporter/token pair yields [`StabilizationConnectionStep::Stale`]
    /// without advancing the cursor. A valid exhausted plan yields `Complete`;
    /// otherwise exactly one candidate is returned and the cursor advances once.
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
///
/// The candidate set combines the local node, current successors, the reported
/// predecessor, and every reported successor. [`successors`] then removes
/// duplicates, orders candidates by clockwise distance, and enforces
/// `capacity`: it is the only truncation, as in Chord's
/// `succ_list ← [s] ++ s.succ_list` followed by keeping `r` entries.
///
/// Law: `∀p ∈ reported. rank(local, known, p) ≤ capacity ⇒ p ∈ result`. A
/// reported list carries no terminal self entry (the reporter answers with
/// its successor sequence verbatim), so dropping its last entry would lose a
/// real successor whenever that list is shorter than `capacity` (#786).
pub fn stabilize_successors(
    local: Did,
    current: &[Did],
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> Vec<Did> {
    let mut known = vec![local];
    for candidate in current
        .iter()
        .copied()
        .chain(topo_predecessor)
        .chain(topo_successors.iter().copied())
    {
        push_unique(&mut known, candidate);
    }
    successors(&known, local, capacity)
}

/// Improved-successor query emitted by one HMCC/Zave stabilize transition.
///
/// A reported predecessor is queried only when it is neither `local` nor at or
/// beyond the current head. This preserves strict clockwise improvement and
/// avoids issuing a redundant query for the existing successor interval.
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
///
/// The nearest normalized successor is notified unless it is the local node.
/// Empty or self-only successor lists therefore produce no network action.
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
///
/// A state with no remote head clears any obsolete request and emits no action.
/// While the head's previous report is still `Processing`, the round is
/// skipped: the handler owning that claim is mid-way through its bounded
/// candidate budget, and superseding it every maintenance period would leave
/// the report unapplied whenever one candidate handshake outlasts the period.
/// Otherwise the exact `(reporter, request_id)` pair is stored in `Requested`
/// phase, superseding an unanswered round, before the matching topology query
/// is emitted.
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
    if state
        .pending_stabilization
        .is_some_and(|pending| pending.phase == StabilizationPhase::Processing)
    {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
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
///
/// Only the exact current reporter and request token may claim the round. A
/// mismatch leaves the state unchanged, while a match reserves the token for
/// bounded connection effects and later application by [`step_stabilize`].
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

/// Apply a successor topology report that owns the current claim.
///
/// The report must own the current `Processing` claim or the entire transition
/// is ignored: there is no token-less path by which an unsolicited report can
/// refine successors. A valid report normalizes successor evidence, emits
/// improvement and notification actions, updates finger hints, proves the
/// local finger range when the reporter remains the head and reports `local`
/// as its predecessor, and retires the token.
pub(super) fn step_stabilize(
    state: &TopologyState,
    reporter: Did,
    request_id: uuid::Uuid,
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> TopologyStep {
    if !state.is_processing_stabilization_report(reporter, request_id) {
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
    // A head report with `pred(head) == local` proves every finger slot whose
    // target lies inside the local successor range.
    let successor_proof_end =
        stabilized_successor_proof_end(state, reporter, &next_successors, topo_predecessor);
    if let Some(end) = successor_proof_end {
        fingers
            .iter_mut()
            .take(end.saturating_add(1))
            .for_each(|finger| *finger = Some(reporter));
    }
    let (fingers, mut finger_convergence) = rehint(state, fingers);
    if let Some(end) = successor_proof_end {
        finger_convergence.confirm_range(0, end);
    }
    TopologyStep {
        state: TopologyState {
            successors: next_successors,
            fingers,
            finger_convergence,
            pending_stabilization: None,
            ..state.clone()
        },
        actions,
    }
}
