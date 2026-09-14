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
//!              [store Requested token]
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
//!                         [cancel/invalidate removes ownership]
//! ```

use std::collections::BTreeMap;

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
    /// Construction removes `local`, preserves first-seen report order, and
    /// truncates the collection to the local successor-list capacity.
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
    /// `successor_capacity`. It does not itself verify the claim; every emitted
    /// effect is guarded again by [`Self::advance`] against live sync state.
    pub(crate) fn new(
        reporter: Did,
        request_id: uuid::Uuid,
        candidates: impl IntoIterator<Item = Did>,
        local: Did,
        successor_capacity: usize,
    ) -> Self {
        let mut bounded = Vec::with_capacity(successor_capacity);
        for candidate in candidates {
            if bounded.len() == successor_capacity {
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
/// request for the same reporter supersedes the older token, and a report must
/// atomically claim the exact token before any connection effect is allowed.
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
    /// This is the topology-churn boundary shared by request creation and report
    /// claiming. Retaining only live reporters prevents a removed successor from
    /// preserving authority through an otherwise valid old token.
    fn retain_current(&mut self, current_successors: &[Did]) {
        self.pending
            .retain(|reporter, _| current_successors.contains(reporter));
    }

    /// Register the exact request sent to a current successor.
    ///
    /// Existing entries for removed successors are pruned first. A non-successor
    /// reporter is rejected; a current reporter receives a new `Requested` token
    /// that deliberately supersedes any older round for the same DID.
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

    /// Invalidate every request claim when the local successor view changes.
    ///
    /// Clearing the map revokes both unclaimed reports and partially consumed
    /// connection plans. Their next call to `advance` observes missing ownership
    /// and returns `Stale` before another network effect can run.
    pub(crate) fn invalidate(&mut self) {
        self.pending.clear();
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
/// The child module exercises stale-token rejection, single-use claims,
/// topology-churn invalidation, and per-report connection-effect bounds.
mod tests {
    use super::SuccessorSyncConnectionPlan;
    use super::SuccessorSyncConnectionStep;
    use super::SuccessorSyncState;
    use crate::dht::Did;

    /// Verifies single-use exact-token claims, per-candidate revocation, and
    /// pruning of requests whose reporters leave the current successor set.
    ///
    /// The test first proves that an older token and a replay cannot claim a
    /// newer round, then invalidates a partially consumed plan and checks that no
    /// second candidate is emitted. Finally it witnesses churn pruning and full
    /// invalidation across two reporters.
    #[test]
    fn exact_claim_is_single_use_and_churn_prunes_old_reporters() {
        let first = Did::from(1u32);
        let second = Did::from(2u32);
        let old = uuid::Uuid::from_u128(1);
        let current = uuid::Uuid::from_u128(2);
        let mut state = SuccessorSyncState::default();

        assert!(state.begin(&[first], first, old));
        assert!(state.begin(&[first], first, current));
        assert!(!state.claim(&[first], first, old));
        assert!(state.claim(&[first], first, current));
        assert!(!state.claim(&[first], first, current));
        let mut plan = SuccessorSyncConnectionPlan::new(
            first,
            current,
            [second, Did::from(3u32)],
            Did::from(0u32),
            2,
        );
        assert_eq!(
            plan.advance(&state, &[first]),
            SuccessorSyncConnectionStep::Connect(second)
        );
        state.invalidate();
        assert_eq!(
            plan.advance(&state, &[first]),
            SuccessorSyncConnectionStep::Stale
        );

        assert!(state.begin(&[first], first, old));
        assert!(state.begin(&[second], second, current));
        assert_eq!(state.pending_count(), 1);
        assert!(!state.claim(&[second], first, old));
        state.invalidate();
        assert!(!state.claim(&[second], second, current));
    }
}
