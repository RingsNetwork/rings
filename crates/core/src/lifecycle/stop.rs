use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Authority that can request cooperative shutdown for one lifecycle scope.
#[derive(Clone, Debug, Default)]
pub struct StopSource {
    requested: Arc<AtomicBool>,
}

impl StopSource {
    /// Create a fresh lifecycle source in the running state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a read-only token linked to this source.
    pub fn token(&self) -> StopToken {
        StopToken {
            requested: self.requested.clone(),
        }
    }

    /// Request shutdown for every token linked to this source.
    ///
    /// This operation is idempotent and monotonic; there is no resume state.
    pub fn request_stop(&self) {
        self.requested.store(true, Ordering::Release);
    }

    /// Return whether this source has requested shutdown.
    pub fn is_stop_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

/// Read-only cooperative shutdown capability for long-running loops.
#[derive(Clone, Debug, Default)]
pub struct StopToken {
    requested: Arc<AtomicBool>,
}

impl StopToken {
    /// Create a token that is never stopped by an external source.
    pub fn never() -> Self {
        Self::default()
    }

    /// Return whether the owner has requested cooperative shutdown.
    pub fn should_stop(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}
