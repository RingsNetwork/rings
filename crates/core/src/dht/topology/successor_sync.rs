//! Correlation state for successor-list synchronization effects.
//!
//! # Algorithm flow
//!
//! ```text
//! [begin(current_successors, reporter, request_id)]
//!                         |
//!                         v
//!              [prune non-successor reporters]
//!                         |
//!                         v
//!              [reporter still current?] -- no --> [reject]
//!                         |
//!                        yes
//!                         v
//!              [reporter's report being processed?] -- yes --> [keep it, reject]
//!                         |
//!                        no
//!                         v
//!              [store Requested token, superseding an unanswered one]
//!                         |
//!                         v
//! [claim(reporter, request_id)] -- mismatch --> [reject as stale]
//!                         |
//!                       exact
//!                         v
//!              [mark token Processing]
//!                         |
//!                         v
//!              [build bounded candidate plan]
//!                         |
//!                         v
//! [advance: reporter current and token Processing?]
//!              |                         |
//!             no                        yes
//!              |                         |
//!              v                         v
//!           [Stale]          [next candidate or Complete]
//!                                        |
//!                                        v
//!                    [cancel removes ownership; successor churn prunes
//!                     the tokens of reporters that left]
//! ```

use std::collections::BTreeMap;

use super::bounded_connection_candidates;
use super::Did;

/// Ownership phase of one successor-list synchronization token.
///
/// The two phases separate an emitted request from a report that has atomically
/// claimed the right to spend the request's bounded connection budget.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum SuccessorSyncPhase {
    /// Query was sent and the first matching report may claim it.
    Requested,
    /// A report claimed the token and may spend its bounded connection budget.
    Processing,
}

/// Exact successor-sync request tracked for one reporter.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct SuccessorSyncRequest {
    /// Fresh correlation token that the authenticated report must echo.
    ///
    /// Replacing this value for a reporter makes every response from an older
    /// synchronization round stale without affecting other current successors.
    request_id: uuid::Uuid,
    /// Whether the report has been reserved by the effect handler.
    ///
    /// Only `Processing` requests may authorize candidate connection effects;
    /// `Requested` accepts exactly one matching claim.
    phase: SuccessorSyncPhase,
}

/// Bounded connection-effect cursor for one claimed successor-sync report.
///
/// The handler and formal model both consume this production transition. It
/// revalidates the claimed token before every candidate and owns the hard
/// per-report effect bound.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct SuccessorSyncConnectionPlan {
    /// Successor that produced the claimed successor-list report.
    ///
    /// The reporter must remain in the current successor set whenever the plan
    /// advances, so topology churn immediately revokes its remaining budget.
    reporter: Did,
    /// Correlation token echoed by the authenticated report.
    ///
    /// The token binds this cursor to one claimed round even if the same
    /// reporter starts another synchronization request later.
    request_id: uuid::Uuid,
    /// Bounded, deduplicated peers reported by the successor.
    ///
    /// Construction removes `local`, preserves the order given, and truncates
    /// the collection to the local successor-list capacity.
    candidates: Vec<Did>,
    /// Cursor for the next candidate whose connection effect may run.
    ///
    /// It advances only when a valid claim emits a candidate, ensuring each
    /// retained candidate consumes at most one unit of the report's budget.
    next_candidate: usize,
}

/// Next permitted effect for a successor-sync connection plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SuccessorSyncConnectionStep {
    /// Connect this bounded candidate, then re-enter the transition.
    Connect(
        /// Candidate authorized by the still-current report claim.
        Did,
    ),
    /// Every candidate was consumed while the claim remained current.
    Complete,
    /// The report was superseded; no further network effect is permitted.
    Stale,
}

impl SuccessorSyncConnectionPlan {
    /// Create a bounded candidate cursor for one claimed successor-sync report.
    ///
    /// The constructor retains the first unique, non-local candidates up to
    /// `successor_capacity`, in the order given (the handler orders them by
    /// transport quality). It does not itself verify the claim; every emitted
    /// effect is guarded again by [`Self::advance`] against live sync state.
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
            candidates: bounded_connection_candidates(local, successor_capacity, candidates),
            next_candidate: 0,
        }
    }

    /// Return the next candidate only while the report claim is still current.
    ///
    /// Missing reporter membership or token ownership yields `Stale` without
    /// consuming a candidate. A valid exhausted cursor yields `Complete`;
    /// otherwise one candidate is returned and the cursor advances exactly once.
    pub(crate) fn advance(
        &mut self,
        state: &SuccessorSyncState,
        current_successors: &[Did],
    ) -> SuccessorSyncConnectionStep {
        if !state.is_processing(current_successors, self.reporter, self.request_id) {
            return SuccessorSyncConnectionStep::Stale;
        }
        let Some(candidate) = self.candidates.get(self.next_candidate).copied() else {
            return SuccessorSyncConnectionStep::Complete;
        };
        self.next_candidate = self.next_candidate.saturating_add(1);
        SuccessorSyncConnectionStep::Connect(candidate)
    }
}

/// Bounded correlation state for successor-list synchronization requests.
///
/// At most one request is retained per current successor. Beginning a newer
/// request for the same reporter supersedes an unanswered token but never a
/// claimed one, and a report must atomically claim the exact token before any
/// connection effect is allowed.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub(crate) struct SuccessorSyncState {
    /// Pending request keyed by reporter DID.
    ///
    /// The map contains at most one token for each current successor. Calls that
    /// receive the current successor set prune entries whose reporters have left.
    pending: BTreeMap<Did, SuccessorSyncRequest>,
}

impl SuccessorSyncState {
    /// Drop requests for peers that are no longer current successors.
    ///
    /// This is the topology-churn boundary: the peer ring applies it whenever a
    /// commit changes the successor list, and request creation and report
    /// claiming apply it again. A token of a reporter that is still a successor
    /// survives churn elsewhere in the list, so a claimed report keeps its
    /// remaining candidate budget when its own admissions extend the list.
    /// Retaining only live reporters prevents a removed successor from
    /// preserving authority through an otherwise valid old token.
    pub(crate) fn retain_current(&mut self, current_successors: &[Did]) {
        self.pending
            .retain(|reporter, _| current_successors.contains(reporter));
    }

    /// Register the exact request sent to a current successor.
    ///
    /// Existing entries for removed successors are pruned first. A non-successor
    /// reporter is rejected. A reporter whose previous report is still being
    /// processed keeps that claim and the new request is rejected, so a
    /// maintenance round cannot revoke the candidate budget of a handler that
    /// is mid-way through it. Otherwise the reporter receives a new `Requested`
    /// token that deliberately supersedes an unanswered older round.
    pub(crate) fn begin(
        &mut self,
        current_successors: &[Did],
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> bool {
        self.retain_current(current_successors);
        if !current_successors.contains(&reporter) {
            return false;
        }
        if self
            .pending
            .get(&reporter)
            .is_some_and(|pending| pending.phase == SuccessorSyncPhase::Processing)
        {
            return false;
        }
        self.pending.insert(reporter, SuccessorSyncRequest {
            request_id,
            phase: SuccessorSyncPhase::Requested,
        });
        true
    }

    /// Atomically claim a report only when its reporter and request identity both match.
    ///
    /// The operation first prunes reporters lost to topology churn, then changes
    /// exactly one matching `Requested` entry to `Processing`. Replays, stale
    /// tokens, unknown reporters, and already claimed reports all return `false`.
    pub(crate) fn claim(
        &mut self,
        current_successors: &[Did],
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> bool {
        self.retain_current(current_successors);
        let Some(pending) = self.pending.get_mut(&reporter) else {
            return false;
        };
        if *pending
            != (SuccessorSyncRequest {
                request_id,
                phase: SuccessorSyncPhase::Requested,
            })
        {
            return false;
        }
        pending.phase = SuccessorSyncPhase::Processing;
        true
    }

    /// Whether this exact successor report still owns its connection budget.
    ///
    /// Ownership requires both current successor membership and an exact
    /// `(reporter, request_id, Processing)` map entry. The predicate is checked
    /// before every candidate effect rather than only when the plan is created.
    fn is_processing(
        &self,
        current_successors: &[Did],
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> bool {
        current_successors.contains(&reporter)
            && self.pending.get(&reporter)
                == Some(&SuccessorSyncRequest {
                    request_id,
                    phase: SuccessorSyncPhase::Processing,
                })
    }

    /// Cancel a request without deleting a newer request for the same reporter.
    ///
    /// Removal occurs only when the stored token equals `request_id`. A delayed
    /// send failure from an older round therefore cannot revoke a newer request
    /// owned by the same successor.
    pub(crate) fn cancel(&mut self, reporter: Did, request_id: uuid::Uuid) {
        if self
            .pending
            .get(&reporter)
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.pending.remove(&reporter);
        }
    }

    #[cfg(test)]
    /// Number of retained successor-sync requests in tests.
    ///
    /// This projection exposes only cardinality, allowing tests to verify churn
    /// pruning without depending on private request phases or token storage.
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
/// Unit tests for exact successor-sync ownership and bounded candidate plans.
///
/// The cases exercise stale-token rejection, single-use claims, the
/// processing-claim protection against a newer round, churn pruning, and the
/// per-report connection-effect bound.
mod tests {
    use super::SuccessorSyncConnectionPlan;
    use super::SuccessorSyncConnectionStep;
    use super::SuccessorSyncState;
    use crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY;
    use crate::dht::Did;

    /// Verifies single-use exact-token claims: a superseded token and a replay
    /// of a claimed token are both rejected.
    #[test]
    fn exact_claim_is_single_use() {
        let first = Did::from(1u32);
        let stale = uuid::Uuid::from_u128(1);
        let current = uuid::Uuid::from_u128(2);
        let mut state = SuccessorSyncState::default();

        // Two begins for the same reporter leave exactly one claimable token.
        assert!(state.begin(&[first], first, stale));
        assert!(state.begin(&[first], first, current));
        assert!(!state.claim(&[first], first, stale));
        assert!(state.claim(&[first], first, current));
        assert!(!state.claim(&[first], first, current));
    }

    /// Verifies that a newer round cannot supersede a report that is being
    /// processed, and that the claim's cancellation reopens the reporter.
    #[test]
    fn begin_keeps_a_processing_claim() {
        let first = Did::from(1u32);
        let claimed = uuid::Uuid::from_u128(1);
        let newer = uuid::Uuid::from_u128(2);
        let mut state = SuccessorSyncState::default();

        assert!(state.begin(&[first], first, claimed));
        assert!(state.claim(&[first], first, claimed));
        assert!(!state.begin(&[first], first, newer));
        let mut plan = SuccessorSyncConnectionPlan::new(
            first,
            claimed,
            [Did::from(2u32)],
            Did::from(0u32),
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        assert_eq!(
            plan.advance(&state, &[first]),
            SuccessorSyncConnectionStep::Connect(Did::from(2u32))
        );

        state.cancel(first, claimed);
        assert!(state.begin(&[first], first, newer));
    }

    /// Verifies the churn law: a plan survives successor-list changes that keep
    /// its reporter (its own admissions extend the list), and is revoked once
    /// the reporter leaves.
    #[test]
    fn churn_prunes_only_departed_reporters() {
        let local = Did::from(0u32);
        let reporter = Did::from(1u32);
        let admitted = Did::from(2u32);
        let request_id = uuid::Uuid::from_u128(1);
        let mut state = SuccessorSyncState::default();
        assert!(state.begin(&[reporter], reporter, request_id));
        assert!(state.claim(&[reporter], reporter, request_id));
        let mut plan = SuccessorSyncConnectionPlan::new(
            reporter,
            request_id,
            [admitted, Did::from(3u32)],
            local,
            DEFAULT_SUCCESSOR_CAPACITY,
        );
        assert_eq!(
            plan.advance(&state, &[reporter]),
            SuccessorSyncConnectionStep::Connect(admitted)
        );

        // Admitting the first candidate changes the successor list; the plan
        // keeps its remaining budget because the reporter is still current.
        state.retain_current(&[reporter, admitted]);
        assert_eq!(
            plan.advance(&state, &[reporter, admitted]),
            SuccessorSyncConnectionStep::Connect(Did::from(3u32))
        );

        // Once the reporter leaves, the token is gone and the plan is stale.
        state.retain_current(&[admitted]);
        assert_eq!(state.pending_count(), 0);
        assert_eq!(
            plan.advance(&state, &[admitted]),
            SuccessorSyncConnectionStep::Stale
        );
        assert!(!state.claim(&[admitted], reporter, request_id));
    }

    /// Verifies that one report admits at most the successor capacity even
    /// when it carries a longer candidate list.
    #[test]
    fn plan_is_bounded_by_successor_capacity() {
        let reporter = Did::from(1u32);
        let request_id = uuid::Uuid::from_u128(1);
        let mut state = SuccessorSyncState::default();
        assert!(state.begin(&[reporter], reporter, request_id));
        assert!(state.claim(&[reporter], reporter, request_id));
        let mut plan = SuccessorSyncConnectionPlan::new(
            reporter,
            request_id,
            (2..=10u32).map(Did::from),
            Did::from(0u32),
            DEFAULT_SUCCESSOR_CAPACITY,
        );

        for _ in 0..DEFAULT_SUCCESSOR_CAPACITY {
            assert!(matches!(
                plan.advance(&state, &[reporter]),
                SuccessorSyncConnectionStep::Connect(_)
            ));
        }
        assert_eq!(
            plan.advance(&state, &[reporter]),
            SuccessorSyncConnectionStep::Complete
        );
    }
}
