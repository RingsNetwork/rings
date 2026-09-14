use std::collections::BTreeMap;

use super::Did;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum SuccessorSyncPhase {
    Requested,
    Processing,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct SuccessorSyncRequest {
    request_id: uuid::Uuid,
    phase: SuccessorSyncPhase,
}

/// Bounded connection-effect cursor for one claimed successor-sync report.
///
/// The handler and formal model both consume this production transition. It
/// revalidates the claimed token before every candidate and owns the hard
/// per-report effect bound.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct SuccessorSyncConnectionPlan {
    reporter: Did,
    request_id: uuid::Uuid,
    candidates: Vec<Did>,
    next_candidate: usize,
}

/// Next permitted effect for a successor-sync connection plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SuccessorSyncConnectionStep {
    /// Connect this bounded candidate, then re-enter the transition.
    Connect(Did),
    /// Every candidate was consumed while the claim remained current.
    Complete,
    /// The report was superseded; no further network effect is permitted.
    Stale,
}

impl SuccessorSyncConnectionPlan {
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
    pending: BTreeMap<Did, SuccessorSyncRequest>,
}

impl SuccessorSyncState {
    fn retain_current(&mut self, current_successors: &[Did]) {
        self.pending
            .retain(|reporter, _| current_successors.contains(reporter));
    }

    /// Register the exact request sent to a current successor.
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
    pub(crate) fn cancel(&mut self, reporter: Did, request_id: uuid::Uuid) {
        if self
            .pending
            .get(&reporter)
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.pending.remove(&reporter);
        }
    }

    /// Invalidate every proof when the local successor view changes.
    pub(crate) fn invalidate(&mut self) {
        self.pending.clear();
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::SuccessorSyncConnectionPlan;
    use super::SuccessorSyncConnectionStep;
    use super::SuccessorSyncState;
    use crate::dht::Did;

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
