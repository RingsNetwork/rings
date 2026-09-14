//! Finger-proof ownership at the transport-admission boundary.

use super::PeerConnectionLifecycle;
use super::PendingConnectionAttempt;
use crate::dht::finger::FingerResultDisposition;

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
    /// No logical connection generation exists for the candidate.
    Missing,
    /// An active generation exists, but its transport cannot make progress.
    Unroutable,
    /// The report did not prove the requested finger threshold.
    Invalid,
    /// The report arrived after the current request deadline.
    Expired,
    /// The report no longer matches the node's current in-flight request.
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

impl From<FingerResultDisposition> for FingerUpdateDisposition {
    fn from(disposition: FingerResultDisposition) -> Self {
        match disposition {
            FingerResultDisposition::Applied { .. } => Self::Applied,
            FingerResultDisposition::Invalid => Self::Invalid,
            FingerResultDisposition::Expired => Self::Expired,
            FingerResultDisposition::Stale => Self::Stale,
        }
    }
}

/// Pure plan for reconciling one finger candidate with a lifecycle snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FingerCandidateAdmission {
    Apply,
    Queue(PendingConnectionAttempt),
    Missing,
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
