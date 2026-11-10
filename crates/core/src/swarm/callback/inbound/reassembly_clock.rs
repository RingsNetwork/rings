//! Epoch clock boundary for chunk reassembly: admission freshness and periodic cleanup.
//!
//! Both decisions read one clock so a test can drive expiry deterministically
//! through the production path instead of waiting for wall time to pass.

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::sync::Arc;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::sync::Mutex;

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use crate::utils::ManualClock;
use crate::utils::SharedClock;

#[derive(Clone)]
pub(in crate::swarm::callback) enum ReassemblyClock {
    /// Read the current Unix epoch timestamp in milliseconds from the ambient system clock.
    #[allow(dead_code)]
    System,
    /// Read a transport- or test-injected [`SharedClock`].
    Injected(SharedClock),
}

impl ReassemblyClock {
    #[allow(dead_code)]
    pub(in crate::swarm::callback) const fn system() -> Self {
        Self::System
    }

    pub(in crate::swarm::callback) fn from_clock(clock: SharedClock) -> Self {
        Self::Injected(clock)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(in crate::swarm::callback) fn controlled(now_ms: Arc<Mutex<u128>>) -> Self {
        Self::Injected(Arc::new(ManualClock::from_shared(now_ms)))
    }

    pub(in crate::swarm::callback) fn now_ms(&self) -> u128 {
        match self {
            Self::System => crate::utils::get_epoch_ms(),
            Self::Injected(clock) => clock.now_ms(),
        }
    }
}
