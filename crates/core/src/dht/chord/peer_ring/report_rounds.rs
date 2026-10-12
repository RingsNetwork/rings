//! The stabilization and successor-sync report rounds of a [`PeerRing`]: a head begins a
//! round, claims the report that answers it, spends the claim's candidate budget, and commits
//! or cancels it.

use super::PeerRing;
use crate::dht::chord::PeerRingAction;
use crate::dht::chord::TopoInfo;
use crate::dht::topology;
use crate::dht::topology::TopologyEvent;
use crate::dht::Did;
use crate::error::Result;

/// The exclusive right to spend one claimed stabilization report's candidate
/// budget and commit it.
///
/// Dropping the claim releases the token, on every exit path of the handler
/// holding it, so a failed or cancelled handler cannot leave the report in
/// `Processing` and block the head's next round (which
/// [`PeerRing::begin_stabilization`] skips while a report is being processed).
/// Release is idempotent: the pure model clears only a token with this exact
/// identity, so a claim whose report was already applied releases nothing.
#[must_use = "dropping the claim releases the report"]
pub(crate) struct StabilizationClaim<'ring> {
    ring: &'ring PeerRing,
    request_id: uuid::Uuid,
}

impl StabilizationClaim<'_> {
    /// The token this claim owns.
    pub(crate) const fn request_id(&self) -> uuid::Uuid {
        self.request_id
    }
}

impl Drop for StabilizationClaim<'_> {
    fn drop(&mut self) {
        // A poisoned lock is the only possible failure, and a destructor has
        // no caller to report it to.
        let _ = self.ring.cancel_stabilization(self.request_id);
    }
}

/// The exclusive right to spend one claimed successor-sync report's
/// candidate budget.
///
/// Dropping the claim releases the token, on every exit path of the handler
/// holding it; see [`StabilizationClaim`] for why that matters.
#[must_use = "dropping the claim releases the report"]
pub(crate) struct SuccessorSyncClaim<'ring> {
    ring: &'ring PeerRing,
    reporter: Did,
    request_id: uuid::Uuid,
}

impl Drop for SuccessorSyncClaim<'_> {
    fn drop(&mut self) {
        let _ = self
            .ring
            .cancel_successor_sync(self.reporter, self.request_id);
    }
}

/// Stabilization rounds.
///
/// A round is correlated by a fresh UUID recorded against the successor head
/// it was sent to; only an authenticated report from that head carrying that
/// UUID may change topology. In order of occurrence:
///
/// 1. [`Self::begin_stabilization`] records `(head, request_id)` as
///    `Requested` and returns the query action. While the head's previous
///    report is still being processed it records nothing and emits nothing.
/// 2. [`Self::claim_stabilization_report`] moves the token to `Processing`
///    exactly once, so duplicate deliveries of one report cannot both spend
///    its connection budget, and hands the caller a [`StabilizationClaim`]
///    that releases the token when dropped.
/// 3. [`Self::advance_stabilization_connection_plan`] hands out at most one
///    candidate per call, re-checking the token each time; churn revokes the
///    remaining budget.
/// 4. [`Self::stabilize_reported_by`] applies the report and returns the
///    follow-up actions; [`Self::cancel_stabilization`] releases the token.
///
/// The pure model clears the token whenever the head changes, so a report
/// answered by a superseded head is stale by construction.
impl PeerRing {
    /// Start one stabilization round against the current head.
    ///
    /// With a head, the transition records `(head, request_id)` and returns
    /// the query action that must carry the same token. Without one, it
    /// records nothing and returns no remote work.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn begin_stabilization(&self, request_id: uuid::Uuid) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::BeginStabilize { request_id })?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Claim a stabilization report before any of its candidate work starts.
    ///
    /// The claim predicate and the claiming step see the same snapshot, so two
    /// handlers racing on one report cannot both succeed: the first commit
    /// moves the token to `Processing`, and the second predicate evaluates
    /// against that state. Returns the claim when this caller won it.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn claim_stabilization_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<Option<StabilizationClaim<'_>>> {
        self.transition(|state, _| {
            let claimable = state.can_claim_stabilization_report(reporter, request_id);
            let step = self.step(state, TopologyEvent::ClaimStabilize {
                reporter,
                request_id,
            });
            (step, claimable)
        })
        // Lazily: constructing a guard for a failed claim would drop it at
        // once and release a token this caller never owned.
        .map(|(_, claimed)| {
            claimed.then(|| StabilizationClaim {
                ring: self,
                request_id,
            })
        })
    }

    /// Reserve the next stabilization candidate against the current topology.
    ///
    /// Each call revalidates the plan's reporter and token before advancing
    /// its cursor: `Connect` for one authorized candidate, `Complete` when
    /// exhausted, `Stale` (with no further effect) after churn.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn advance_stabilization_connection_plan(
        &self,
        plan: &mut topology::ConnectionPlan,
    ) -> Result<topology::ConnectionStep> {
        self.with_topology_state(|state| {
            plan.advance(|reporter, request_id| {
                state.is_processing_stabilization_report(reporter, request_id)
            })
        })
    }

    /// Apply a topology report from the owner of the matching stabilization token.
    ///
    /// The pure model rechecks `reporter` and `request_id`, merges the reported
    /// successor and predecessor evidence, and returns every follow-up action
    /// as a batch. A stale token yields no state change and no work.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn stabilize_reported_by(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
        info: TopoInfo,
    ) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::Stabilize {
            reporter,
            request_id,
            successors: info.successors,
            predecessor: info.predecessor,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }

    /// Release the stabilization token `request_id`, and only that one.
    ///
    /// The pure model compares tokens before clearing, so an old cancellation
    /// cannot retire a newer round.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn cancel_stabilization(&self, request_id: uuid::Uuid) -> Result<()> {
        self.transition_topology(TopologyEvent::CancelStabilize { request_id })
            .map(|_| ())
    }
}

/// Successor-list synchronization.
///
/// Each successor may have one outstanding successor-list query, owned by a
/// `(reporter, request_id)` token in [`topology::SuccessorSyncState`]. A
/// token is valid only while its reporter is still a successor: the
/// transition core prunes departed reporters' tokens when a commit changes
/// the successor list, and each operation here reads the list under the same
/// lock it uses to touch the tokens.
impl PeerRing {
    /// Operate on the sync tokens and the successor list they are judged
    /// against, from one committed topology.
    fn with_successor_sync<T>(
        &self,
        operate: impl FnOnce(&mut topology::SuccessorSyncState, &[Did]) -> T,
    ) -> Result<T> {
        let _transition = self.lock_transition()?;
        let successors = self.successor_seq.list()?;
        let mut pending = self.lock_pending_successor_sync()?;
        Ok(operate(&mut pending, &successors))
    }

    /// Register one sync token for a current successor.
    ///
    /// Succeeds only while `reporter` is a successor and its previous report
    /// is not still being processed, replacing an unanswered older token;
    /// failure creates no report authority.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn begin_successor_sync(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<bool> {
        self.with_successor_sync(|pending, successors| {
            pending.begin(successors, reporter, request_id)
        })
    }

    /// Claim one matching sync report before it can create effects.
    ///
    /// Succeeds once for the exact `(reporter, request_id)` pair while the
    /// reporter is still a successor, handing the caller a
    /// [`SuccessorSyncClaim`] that releases the token when dropped. Duplicate,
    /// replaced, and departed-reporter reports return `None` and acquire no
    /// connection budget.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn claim_successor_sync_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<Option<SuccessorSyncClaim<'_>>> {
        // Lazily: a guard built for a failed claim would drop inside the lock
        // and its release would deadlock on it.
        self.with_successor_sync(|pending, successors| {
            pending
                .claim(successors, reporter, request_id)
                .then(|| SuccessorSyncClaim {
                    ring: self,
                    reporter,
                    request_id,
                })
        })
    }

    /// Reserve one sync candidate after revalidating live ownership.
    ///
    /// The cursor advances only for a still-processing token whose reporter
    /// is still a successor: `Connect` permits one connection, `Complete`
    /// reports exhaustion, `Stale` revokes the remaining work.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn advance_successor_sync_connection_plan(
        &self,
        plan: &mut topology::ConnectionPlan,
    ) -> Result<topology::ConnectionStep> {
        self.with_successor_sync(|pending, successors| {
            plan.advance(|reporter, request_id| {
                pending.is_processing(successors, reporter, request_id)
            })
        })
    }

    /// Release the exact sync token `(reporter, request_id)`.
    ///
    /// Cancellation does not consult the successor list: a token must remain
    /// releasable after its reporter has churned out, and a mismatched token
    /// is left untouched, so a delayed send or join failure can race a newer
    /// round safely.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn cancel_successor_sync(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<()> {
        let _transition = self.lock_transition()?;
        self.lock_pending_successor_sync()?
            .cancel(reporter, request_id);
        Ok(())
    }
}
