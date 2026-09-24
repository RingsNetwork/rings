//! Detached first-frame admission layered above backend send admission.
//!
//! `Pending -> Irrevocable -> Accepted` publishes first-frame success.
//! `Pending -> Cancelled` wins cancellation before the backend boundary, while
//! `Irrevocable -> Cancelled` is an explicit rollback allowed only when backend
//! admission did not succeed. The shared transport state model defines these
//! edges; this wrapper adds the stop signal required by detached payload work.
//!
//! A detached caller observes `Cancelled` only through `cancelled_outcome`, so
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

#[derive(Clone)]
pub(in crate::swarm::transport) struct DetachedAdmission {
    state: AtomicAdmission,
    stop: StopSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum DetachedAdmissionCancel {
    Cancelled,
    MustAwait,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum DetachedAdmissionClaim {
    New,
    Existing,
}

impl DetachedAdmission {
    pub(in crate::swarm::transport) fn new() -> Self {
        Self {
            state: AtomicAdmission::new(),
            stop: StopSource::new(),
        }
    }

    pub(in crate::swarm::transport) fn stop_token(&self) -> StopToken {
        self.stop.token()
    }

    pub(in crate::swarm::transport) fn cancel(&self) -> DetachedAdmissionCancel {
        match self.state.try_transition(AdmissionEvent::Cancel) {
            Ok(_) | Err(AdmissionPhase::Cancelled) => {
                self.stop.request_stop();
                DetachedAdmissionCancel::Cancelled
            }
            Err(_) => DetachedAdmissionCancel::MustAwait,
        }
    }

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

    pub(in crate::swarm::transport) fn rollback_irrevocable_send(&self) {
        if self.state.try_transition(AdmissionEvent::Rollback).is_ok() {
            self.stop.request_stop();
        }
    }

    /// The outcome a detached caller may observe for a transfer to `peer` that ended
    /// `Cancelled`.
    ///
    /// Post: `Ok(Cancelled)` iff the admission is (or has now been moved to) `Cancelled`, so no
    /// claim can ever succeed; once a claim won (`Irrevocable` or `Accepted`), the backend may
    /// hold the frame, and the outcome is the ambiguous `DetachedSendAbandonedAfterClaim`.
    pub(in crate::swarm::transport) fn cancelled_outcome(
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

    pub(in crate::swarm::transport) fn try_succeed(&self) -> bool {
        self.state.try_transition(AdmissionEvent::Accept).is_ok()
    }

    pub(in crate::swarm::transport) fn enforce_cancelled_stop(&self) {
        self.stop.request_stop();
    }
}
