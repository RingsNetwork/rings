use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::lifecycle::StopSource;
use crate::lifecycle::StopToken;

const PENDING: u8 = 0;
const CANCELLED: u8 = 1;
const IRREVOCABLE: u8 = 2;
const SUCCEEDED: u8 = 3;

#[derive(Clone)]
pub(in crate::swarm::transport) struct DetachedAdmission {
    state: Arc<AtomicU8>,
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
            state: Arc::new(AtomicU8::new(PENDING)),
            stop: StopSource::new(),
        }
    }

    pub(in crate::swarm::transport) fn stop_token(&self) -> StopToken {
        self.stop.token()
    }

    pub(in crate::swarm::transport) fn cancel(&self) -> DetachedAdmissionCancel {
        match self
            .state
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(CANCELLED) => {
                self.stop.request_stop();
                DetachedAdmissionCancel::Cancelled
            }
            Err(_) => DetachedAdmissionCancel::MustAwait,
        }
    }

    pub(in crate::swarm::transport) fn try_mark_irrevocable(
        &self,
    ) -> Option<DetachedAdmissionClaim> {
        match self
            .state
            .compare_exchange(PENDING, IRREVOCABLE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(DetachedAdmissionClaim::New),
            Err(IRREVOCABLE | SUCCEEDED) => Some(DetachedAdmissionClaim::Existing),
            Err(_) => None,
        }
    }

    pub(in crate::swarm::transport) fn rollback_irrevocable_send(&self) {
        if self
            .state
            .compare_exchange(IRREVOCABLE, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.stop.request_stop();
        }
    }

    pub(in crate::swarm::transport) fn try_succeed(&self) -> bool {
        self.state
            .compare_exchange(IRREVOCABLE, SUCCEEDED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(in crate::swarm::transport) fn enforce_cancelled_stop(&self) {
        self.stop.request_stop();
    }
}
