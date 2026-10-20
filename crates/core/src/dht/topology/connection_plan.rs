//! Bounded connection plans of claimed topology reports.
//!
//! A successor-list sync or stabilization report names peers to connect to. The report is
//! untrusted and of any length, so the effects it may cause are bounded here, once: a plan
//! keeps at most `budget` distinct candidates and yields each at most once, and only while the
//! token machine that owns the report still claims it.

use super::push_unique;
use crate::dht::Did;

/// The bounded connection budget of one topology report.
///
/// Keeps the first `capacity` distinct peers of `candidates`, in the order
/// given, skipping `local`. Every report-driven connection plan is built from
/// this one definition, so an untrusted report of any length can cause at most
/// `capacity` connection effects: the successor capacity for a successor-list
/// sync report, one more for a stabilization report's predecessor.
pub(crate) fn bounded_connection_candidates(
    local: Did,
    capacity: usize,
    candidates: impl IntoIterator<Item = Did>,
) -> Vec<Did> {
    let mut bounded = Vec::with_capacity(capacity);
    for candidate in candidates {
        if bounded.len() == capacity {
            break;
        }
        if candidate != local {
            push_unique(&mut bounded, candidate);
        }
    }
    bounded
}

/// The connection budget of one stabilization report: the successor-list
/// capacity plus one slot for the reported predecessor.
pub(crate) const fn stabilization_connection_budget(successor_capacity: usize) -> usize {
    successor_capacity.saturating_add(1)
}

/// Bounded connection-effect cursor for one claimed topology report.
///
/// The handlers and the formal models both consume this production
/// transition. The claim predicate is supplied by the token machine that owns
/// the report (stabilization or successor sync) and is re-evaluated before
/// every candidate, so a superseded round can emit no further effect; the plan
/// itself owns the hard per-report effect bound.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct ConnectionPlan {
    /// Successor that produced the claimed topology report.
    reporter: Did,
    /// Correlation token echoed by the authenticated topology report.
    ///
    /// The token distinguishes this plan from older and newer rounds that
    /// queried the same reporter.
    request_id: uuid::Uuid,
    /// Bounded, deduplicated peers reported by the successor, in the order the
    /// handler ranked them.
    candidates: Vec<Did>,
    /// Cursor for the next candidate whose connection effect may run.
    ///
    /// The cursor advances only after the claim is revalidated and a candidate
    /// is returned, so each bounded candidate is emitted at most once.
    next_candidate: usize,
}

/// Next permitted effect for a connection plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionStep {
    /// Connect this bounded candidate, then re-enter the transition.
    Connect(Did),
    /// Every candidate was consumed while the claim remained current.
    Complete,
    /// The report was superseded; no further network effect is permitted.
    Stale,
}

impl ConnectionPlan {
    /// Create a bounded candidate cursor for one claimed report.
    ///
    /// Candidates are consumed in the order given after removing the local DID
    /// and duplicates; the stored list is capped at `budget`, so one report can
    /// cause at most `budget` connection effects.
    pub(crate) fn new(
        reporter: Did,
        request_id: uuid::Uuid,
        candidates: impl IntoIterator<Item = Did>,
        local: Did,
        budget: usize,
    ) -> Self {
        Self {
            reporter,
            request_id,
            candidates: bounded_connection_candidates(local, budget, candidates),
            next_candidate: 0,
        }
    }

    /// The token whose budget this plan spends.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) const fn request_id(&self) -> uuid::Uuid {
        self.request_id
    }

    /// Return the next candidate only while `still_processing(reporter, request_id)`.
    ///
    /// A superseded claim yields [`ConnectionStep::Stale`] without advancing the
    /// cursor. A valid exhausted plan yields `Complete`; otherwise exactly one
    /// candidate is returned and the cursor advances once.
    pub(crate) fn advance(
        &mut self,
        still_processing: impl FnOnce(Did, uuid::Uuid) -> bool,
    ) -> ConnectionStep {
        if !still_processing(self.reporter, self.request_id) {
            return ConnectionStep::Stale;
        }
        let Some(candidate) = self.candidates.get(self.next_candidate).copied() else {
            return ConnectionStep::Complete;
        };
        self.next_candidate = self.next_candidate.saturating_add(1);
        ConnectionStep::Connect(candidate)
    }
}
