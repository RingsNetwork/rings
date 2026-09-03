//! Epoch clock boundary for chunk reassembly: admission freshness and periodic cleanup.
//!
//! Both decisions read one clock so a test can drive expiry deterministically
//! through the production path instead of waiting for wall time to pass.

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::sync::Arc;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::sync::Mutex;

#[derive(Clone)]
pub(in crate::swarm::callback) enum ReassemblyClock {
    /// Read the current Unix epoch timestamp in milliseconds.
    System,
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    /// Read a test-controlled Unix epoch timestamp in milliseconds.
    Controlled(Arc<Mutex<u128>>),
}

impl ReassemblyClock {
    pub(in crate::swarm::callback) const fn system() -> Self {
        Self::System
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(in crate::swarm::callback) fn controlled(now_ms: Arc<Mutex<u128>>) -> Self {
        Self::Controlled(now_ms)
    }

    pub(super) fn now_ms(&self) -> u128 {
        match self {
            Self::System => crate::utils::get_epoch_ms(),
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            Self::Controlled(now_ms) => *now_ms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }
}
