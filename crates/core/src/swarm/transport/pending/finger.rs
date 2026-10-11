//! Finger-proof ownership at the transport-admission boundary.
//!
//! Finger reports arrive through DHT messages, but a reported successor is only
//! useful after there is a transport generation that can carry traffic to it.
//! This module holds the pure lifecycle classification and the transport's
//! finger-candidate transition that interprets it under the lifecycle boundary.
//!
//! # Algorithm flow
//!
//! ```text
//! Finger report + correlated request
//!                 |
//!                 v
//!       Inspect peer lifecycle state
//!                 |
//!       +---------+----------+------------------+
//!       |                    |                  |
//! Pending/Admitting       Active             Missing
//!       |                    |                  |
//!       v              Is transport             v
//! Queue on exact       routable?          Defer proof and
//! generation          /         \         request connection
//!                    yes         no
//!                     |           |
//!                     v           v
//!               Apply proof   Retire unusable
//!                             candidate proof
//!       \_____________________|__________________/
//!                             |
//!                             v
//!     Applied / Queued / Missing / Unroutable / Rejected(reason)
//! ```

use super::PeerConnectionLifecycle;
use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerDeferOutcome;
use crate::dht::finger::FingerReportRejection;
use crate::dht::finger::FingerRetireOutcome;
use crate::dht::Did;
use crate::dht::FingerFixRequest;
use crate::error::Result;

/// Result of reconciling one reported finger with connection ownership.
///
/// The variants make the state transition exhaustive: a candidate is either
/// committed, retained by the current handshake, retained without an owner,
/// retired because its active transport is not routable, or rejected by the
/// DHT's own validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerUpdateDisposition {
    /// The candidate was committed to the finger table.
    Applied,
    /// The candidate was attached to the current pending generation.
    Queued,
    /// The proof is retained but no logical connection generation owns the
    /// candidate: the caller must open one, or release the proof.
    Missing,
    /// An active generation exists, but its transport cannot make progress and
    /// the retained proof has been retired.
    Unroutable,
    /// The DHT rejected the report; nothing is retained and no connection may
    /// be opened for it.
    Rejected(FingerReportRejection),
}

impl FingerUpdateDisposition {
    /// Whether a retained proof is waiting for a connection generation to own it.
    ///
    /// Only [`Self::Missing`] is in that state. Before connection preparation
    /// it authorizes opening the connection; after preparation it means the
    /// proof still has no owner and must be released, because an admission
    /// proof cannot survive without a transport generation that will admit or
    /// cancel it. Validation failures and an already-owned but unroutable peer
    /// retain nothing and must never start a replacement connection.
    pub(crate) const fn proof_awaits_owner(self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl From<FingerApplyOutcome> for FingerUpdateDisposition {
    /// Map a direct DHT apply result into the message handler's transport decision.
    fn from(outcome: FingerApplyOutcome) -> Self {
        match outcome {
            FingerApplyOutcome::Applied => Self::Applied,
            FingerApplyOutcome::Rejected(rejection) => Self::Rejected(rejection),
        }
    }
}

/// Pure plan for reconciling one finger candidate with a lifecycle snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FingerCandidateAdmission {
    /// Apply the proof immediately because the active transport is routable.
    Apply,
    /// Attach the proof to a pending or admitting generation.
    Queue(
        /// Exact generation that owns cancellation or commit of the deferred proof.
        PendingConnectionAttempt,
    ),
    /// No generation owns the peer, so the caller may open a connection.
    Missing,
    /// A generation owns the peer but cannot carry traffic.
    Unroutable,
}

/// Select the transport-side effect for one finger candidate lifecycle snapshot.
///
/// Pre: `is_routable` describes the active transport owned by `lifecycle`.
/// Pending and admitting states ignore it because their exact generation must
/// retain the proof until admission either commits or is cancelled.
///
/// Post: every lifecycle state maps to exactly one effect plan, and this pure
/// classifier performs no DHT mutation, connection creation, or proof transfer.
pub(super) fn finger_candidate_admission(
    lifecycle: Option<PeerConnectionLifecycle>,
    is_routable: bool,
) -> FingerCandidateAdmission {
    match lifecycle {
        Some(
            PeerConnectionLifecycle::Pending { attempt, .. }
            | PeerConnectionLifecycle::Admitting { attempt, .. },
        ) => FingerCandidateAdmission::Queue(attempt),
        Some(PeerConnectionLifecycle::Active { .. }) if is_routable => {
            FingerCandidateAdmission::Apply
        }
        Some(PeerConnectionLifecycle::Active { .. }) => FingerCandidateAdmission::Unroutable,
        None => FingerCandidateAdmission::Missing,
    }
}

impl SwarmTransport {
    /// Apply one finger candidate or retain its proof until its current handshake commits.
    ///
    /// `request` identifies the specific finger lookup being satisfied. The
    /// returned disposition tells the message handler whether it should open a
    /// transport connection or cancel a proof that could not gain an owner.
    pub(crate) fn record_finger_candidate(
        &self,
        peer: Did,
        request: FingerFixRequest,
    ) -> Result<FingerUpdateDisposition> {
        self.record_finger_candidate_with_observer(peer, request, || {})
    }

    /// Classify and either apply, retain, or reject a reported finger candidate.
    ///
    /// The lifecycle boundary covers the snapshot, proof transfer, and queue
    /// attachment so retirement cannot change the owning generation between
    /// validation and commit. `observe_admission` is a test synchronization
    /// hook invoked only after the branch is selected and before its mutation.
    fn record_finger_candidate_with_observer(
        &self,
        peer: Did,
        request: FingerFixRequest,
        observe_admission: impl FnOnce(),
    ) -> Result<FingerUpdateDisposition> {
        // The lifecycle guard keeps classification, proof transfer, and queue
        // attachment in one critical section. No async network effect occurs
        // while it is held.
        let _lifecycle_guard = self.connection_lifecycle()?;
        if peer == self.dht.did {
            return self
                .dht
                .apply_fixed_finger(request, peer)
                .map(FingerUpdateDisposition::from);
        }
        let (lifecycle, active) = {
            let lifecycles = self.peer_lifecycles()?;
            (lifecycles.state(peer), lifecycles.active_connections())
        };
        let is_routable = self.is_routable_active_candidate(peer, &active);
        match finger_candidate_admission(lifecycle, is_routable) {
            FingerCandidateAdmission::Queue(current) => {
                observe_admission();
                // Acquire the queue before transferring proof ownership. If
                // this lock fails, the proof remains AwaitingReport rather
                // than becoming an admission proof with no transport owner.
                let mut pending_updates = self.pending_finger_updates()?;
                match self.dht.defer_fixed_finger(request, peer)? {
                    FingerDeferOutcome::Deferred => {
                        // The DHT owns one attempt at a time, so a generation
                        // retains one proof; a newer request replaces one the
                        // DHT has already released.
                        pending_updates.insert(current, request);
                        Ok(FingerUpdateDisposition::Queued)
                    }
                    FingerDeferOutcome::Rejected(rejection) => {
                        Ok(FingerUpdateDisposition::Rejected(rejection))
                    }
                }
            }
            FingerCandidateAdmission::Apply => {
                observe_admission();
                self.dht
                    .apply_fixed_finger(request, peer)
                    .map(FingerUpdateDisposition::from)
            }
            FingerCandidateAdmission::Missing => {
                observe_admission();
                // The report becomes a durable, expiring admission proof
                // before the caller starts an async WebRTC handshake. A
                // cancelled or hung handler therefore cannot strand it.
                self.dht
                    .defer_fixed_finger(request, peer)
                    .map(|outcome| match outcome {
                        FingerDeferOutcome::Deferred => FingerUpdateDisposition::Missing,
                        FingerDeferOutcome::Rejected(rejection) => {
                            FingerUpdateDisposition::Rejected(rejection)
                        }
                    })
            }
            FingerCandidateAdmission::Unroutable => {
                // Validate token and successor in the same topology
                // transition that retires the unusable candidate. Checking
                // only the token here would let a conflicting duplicate evict
                // a different successor's retained admission proof.
                match self.dht.retire_finger_candidate(request, peer)? {
                    FingerRetireOutcome::Retired => Ok(FingerUpdateDisposition::Unroutable),
                    FingerRetireOutcome::Rejected(rejection) => {
                        Ok(FingerUpdateDisposition::Rejected(rejection))
                    }
                }
            }
        }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn record_finger_candidate_with_observer_for_test(
        &self,
        peer: Did,
        request: FingerFixRequest,
        observe_admission: impl FnOnce(),
    ) -> Result<FingerUpdateDisposition> {
        self.record_finger_candidate_with_observer(peer, request, observe_admission)
    }
}
