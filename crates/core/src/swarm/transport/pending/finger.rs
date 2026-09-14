//! Finger-proof ownership at the transport-admission boundary.
//!
//! Finger reports arrive through DHT messages, but a reported successor is only
//! useful after there is a transport generation that can carry traffic to it.
//! This module keeps the pure lifecycle classification separate from the DHT
//! proof transitions in `pending.rs`.

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
    /// Whether the caller should start a connection before retrying admission.
    pub(crate) const fn needs_connection(self) -> bool {
        matches!(self, Self::Missing)
    }

    /// Whether connection preparation left a retained proof without a
    /// lifecycle generation that can eventually admit or cancel it.
    pub(crate) const fn leaves_deferred_unowned(self) -> bool {
        matches!(self, Self::Missing | Self::Unroutable)
    }
}

impl From<FingerReportRejection> for FingerUpdateDisposition {
    /// Map DHT proof rejection into a transport-level non-admission disposition.
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
    /// Map a direct DHT apply outcome into the message handler's transport decision.
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
    /// Attach the proof to this pending or admitting generation.
    Queue(PendingConnectionAttempt),
    /// No generation owns the peer, so the caller may open a connection.
    Missing,
    /// A generation owns the peer but cannot carry traffic.
    Unroutable,
}

// Pre: `is_routable` describes the transport owned by `lifecycle`.
// Post: every lifecycle state maps to exactly one effect plan.
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
