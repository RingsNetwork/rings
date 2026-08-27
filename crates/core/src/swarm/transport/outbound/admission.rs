//! Detached first-frame admission layered above backend send admission.
//!
//! `Pending -> Irrevocable -> Accepted` publishes first-frame success.
//! `Pending -> Cancelled` wins cancellation before the backend boundary, while
//! `Irrevocable -> Cancelled` is an explicit rollback allowed only when backend
//! admission did not succeed. The shared transport state model defines these
//! edges; this wrapper adds the stop signal required by detached payload work.

use rings_transport::core::admission::AdmissionEvent;
use rings_transport::core::admission::AdmissionPhase;
use rings_transport::core::admission::AtomicAdmission;

use crate::lifecycle::StopSource;
use crate::lifecycle::StopToken;

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

    pub(in crate::swarm::transport) fn try_succeed(&self) -> bool {
        self.state.try_transition(AdmissionEvent::Accept).is_ok()
    }

    pub(in crate::swarm::transport) fn enforce_cancelled_stop(&self) {
        self.stop.request_stop();
    }
}
