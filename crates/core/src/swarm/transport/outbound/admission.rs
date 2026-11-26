//! Detached first-frame admission layered above backend send admission.
//!
//! `Pending -> Irrevocable -> Accepted` publishes first-frame success.
//! `Pending -> Cancelled` wins cancellation before the backend boundary, while
//! `Irrevocable -> Cancelled` is an explicit rollback allowed only when backend
//! admission did not succeed. The shared transport state model defines these
//! edges; this wrapper adds the stop signal required by detached payload work.
//!
//! A detached caller observes `Cancelled` only through `settle_cancelled`, so
//! `Cancelled` observed ⇒ no claim ever succeeded (lemma (P) of `error::send_class`).

use rings_transport::core::admission::AdmissionEvent;
use rings_transport::core::admission::AdmissionPhase;
use rings_transport::core::admission::AtomicAdmission;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopSource;
use crate::lifecycle::StopToken;
use crate::swarm::transport::delivery::SendCompletionOutcome;

/// Admission state and stop signal coordinating a detached outbound transfer.
#[derive(Clone)]
pub(in crate::swarm::transport) struct DetachedAdmission {
    /// Atomic state machine tracking first-frame admission and cancellation.
    state: AtomicAdmission,
    /// Stop source triggered when the detached transfer is cancelled.
    stop: StopSource,
}

/// Result of attempting to cancel a detached transfer before backend admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum DetachedAdmissionCancel {
    /// Cancellation won before the backend boundary and stop was requested.
    Cancelled,
    /// Admission already became irrevocable or accepted, so the caller must await its outcome.
    MustAwait,
}

/// Result of claiming irrevocable backend admission for a detached transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum DetachedAdmissionClaim {
    /// This call transitioned the admission from pending to irrevocable.
    New,
    /// Admission was already irrevocable or accepted prior to this call.
    Existing,
}

impl DetachedAdmission {
    /// Create a new pending detached admission with a fresh stop source.
    pub(in crate::swarm::transport) fn new() -> Self {
        Self {
            state: AtomicAdmission::new(),
            stop: StopSource::new(),
        }
    }

    /// Return a stop token that fires if this detached transfer is cancelled.
    pub(in crate::swarm::transport) fn stop_token(&self) -> StopToken {
        self.stop.token()
    }

    /// Attempt to cancel the admission, requesting stop if cancellation succeeds.
    pub(in crate::swarm::transport) fn cancel(&self) -> DetachedAdmissionCancel {
        match self.state.try_transition(AdmissionEvent::Cancel) {
            Ok(_) | Err(AdmissionPhase::Cancelled) => {
                self.stop.request_stop();
                DetachedAdmissionCancel::Cancelled
            }
            Err(_) => DetachedAdmissionCancel::MustAwait,
        }
    }

    /// Try to mark the first frame as irrevocable at the backend send boundary.
    pub(in crate::swarm::transport) fn try_mark_irrevocable(
        &self,
    ) -> Option<DetachedAdmissionClaim> {
        match self.state.try_transition(AdmissionEvent::MarkIrrevocable) {
            Ok(_) => Some(DetachedAdmissionClaim::New),
            Err(AdmissionPhase::Irrevocable | AdmissionPhase::Accepted) => {
                Some(DetachedAdmissionClaim::Existing)
            }
            Err(_) => None,
        }
    }

    /// Roll back an irrevocable claim after backend admission fails, requesting stop.
    pub(in crate::swarm::transport) fn rollback_irrevocable_send(&self) {
        if self.state.try_transition(AdmissionEvent::Rollback).is_ok() {
            self.stop.request_stop();
        }
    }

    /// Settle a detached transfer to `peer` that ended `Cancelled`: cancel the admission (a
    /// compare-and-swap that also requests stop) and return what the caller may observe.
    ///
    /// Post: `Ok(Cancelled)` iff the admission is (or has now been moved to) `Cancelled`, so no
    /// claim can ever succeed; once a claim won (`Irrevocable` or `Accepted`), the backend may
    /// hold the frame, and the outcome is the ambiguous `DetachedSendAbandonedAfterClaim`.
    pub(in crate::swarm::transport) fn settle_cancelled(
        &self,
        peer: Did,
    ) -> Result<SendCompletionOutcome> {
        match self.cancel() {
            DetachedAdmissionCancel::Cancelled => Ok(SendCompletionOutcome::Cancelled),
            DetachedAdmissionCancel::MustAwait => {
                Err(Error::DetachedSendAbandonedAfterClaim { peer })
            }
        }
    }

    /// Mark the detached transfer's first frame as accepted by the backend.
    pub(in crate::swarm::transport) fn try_succeed(&self) -> bool {
        self.state.try_transition(AdmissionEvent::Accept).is_ok()
    }

    /// Request stop to ensure detached work observes a cancellation.
    pub(in crate::swarm::transport) fn enforce_cancelled_stop(&self) {
        self.stop.request_stop();
    }
}
