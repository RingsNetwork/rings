//! Finger-proof ownership at the transport-admission boundary.
//!
//! Finger reports arrive through DHT messages, but a reported successor is only
//! useful after there is a transport generation that can carry traffic to it.
//! This module keeps the pure lifecycle classification separate from the DHT
//! proof transitions in `pending.rs`.
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
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerReportRejection;

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
