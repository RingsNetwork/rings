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
//!            Applied / Queued / Missing / Unroutable
//!                             |
//!               DHT validation rejection maps to
//!                    Invalid / Expired / Stale
//! ```

use super::PeerConnectionLifecycle;
use super::PendingConnectionAttempt;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerReportRejection;

/// Result of reconciling one reported finger with connection ownership.
///
/// The variants make the state transition exhaustive: a candidate is either
/// committed, retained by the current handshake, absent, or owned by an active
/// transport that is not presently routable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerUpdateDisposition {
    /// The candidate was committed to the finger table.
    Applied,
    /// The candidate was attached to the current pending generation.
    Queued,
    /// No logical connection generation exists for the candidate, so the caller
    /// may attempt to open one.
    Missing,
    /// An active generation exists, but its transport cannot make progress and
    /// the retained proof has been retired.
    Unroutable,
    /// The report did not prove the requested finger threshold and must not
    /// trigger connection admission.
    Invalid,
    /// The report arrived after the current request deadline and must not
    /// trigger connection admission.
    Expired,
    /// The report no longer matches the node's current in-flight request and
    /// must not trigger connection admission.
    Stale,
}

impl FingerUpdateDisposition {
    /// Return whether transport admission must create a connection for the candidate.
    ///
    /// Only [`Self::Missing`] authorizes that side effect. Validation failures
    /// and an already-owned but unroutable peer must never start a replacement
    /// connection from this report.
    pub(crate) const fn needs_connection(self) -> bool {
        matches!(self, Self::Missing)
    }

    /// Whether connection preparation left a retained proof without a
    /// lifecycle generation that can eventually admit or cancel it.
    ///
    /// The message handler uses this predicate after connection preparation.
    /// A `true` result requires explicit DHT cancellation so an admission proof
    /// cannot survive without a transport generation that owns it.
    pub(crate) const fn leaves_deferred_unowned(self) -> bool {
        matches!(self, Self::Missing | Self::Unroutable)
    }
}

impl From<FingerReportRejection> for FingerUpdateDisposition {
    /// Map a DHT proof rejection into the equivalent transport-level outcome.
    ///
    /// The mapping preserves the rejection reason and deliberately produces no
    /// connection-admission state, allowing callers to report the exact cause
    /// without reopening or retaining a transport.
    fn from(rejection: FingerReportRejection) -> Self {
        // Rejections are terminal for transport admission: none of these states
        // should cause the caller to open or retain a connection.
        match rejection {
            FingerReportRejection::Invalid => Self::Invalid,
            FingerReportRejection::Expired => Self::Expired,
            FingerReportRejection::Stale => Self::Stale,
        }
    }
}

impl From<FingerApplyOutcome> for FingerUpdateDisposition {
    /// Map a direct DHT apply result into the message handler's transport decision.
    ///
    /// Successful DHT mutation becomes [`Self::Applied`]. A rejected mutation
    /// retains its validation category through the preceding rejection
    /// conversion.
    fn from(outcome: FingerApplyOutcome) -> Self {
        // `Applied` is the only success state; DHT-level validation failures
        // remain visible to the message handler as non-connection dispositions.
        match outcome {
            FingerApplyOutcome::Applied { .. } => Self::Applied,
            FingerApplyOutcome::Rejected(rejection) => rejection.into(),
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
        Some(PeerConnectionLifecycle::Active(_)) if is_routable => FingerCandidateAdmission::Apply,
        Some(PeerConnectionLifecycle::Active(_)) => FingerCandidateAdmission::Unroutable,
        None => FingerCandidateAdmission::Missing,
    }
}
