use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use event_listener::Event;

#[derive(Default)]
struct StopState {
    requested: AtomicBool,
    changed: Event,
}

/// Authority that can request cooperative shutdown for one lifecycle scope.
#[derive(Clone, Default)]
pub struct StopSource {
    state: Arc<StopState>,
}

impl StopSource {
    /// Create a fresh lifecycle source in the running state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a read-only token linked to this source.
    pub fn token(&self) -> StopToken {
        StopToken {
            state: Arc::clone(&self.state),
        }
    }

    /// Request shutdown for every token linked to this source.
    ///
    /// This operation is idempotent and monotonic; there is no resume state.
    pub fn request_stop(&self) {
        if !self.state.requested.swap(true, Ordering::AcqRel) {
            self.state.changed.notify(usize::MAX);
        }
    }

    /// Return whether this source has requested shutdown.
    pub fn is_stop_requested(&self) -> bool {
        self.state.requested.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for StopSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StopSource")
            .field("requested", &self.is_stop_requested())
            .finish()
    }
}

/// Read-only cooperative shutdown capability for long-running loops.
#[derive(Clone, Default)]
pub struct StopToken {
    state: Arc<StopState>,
}

impl StopToken {
    /// Create a token that is never stopped by an external source.
    pub fn never() -> Self {
        Self::default()
    }

    /// Return whether the owner has requested cooperative shutdown.
    pub fn should_stop(&self) -> bool {
        self.state.requested.load(Ordering::Acquire)
    }

    /// Wait until the linked source requests shutdown.
    ///
    /// Registration happens before the second predicate check, so a request racing with this
    /// method cannot be missed. Calling this method on [`Self::never`] waits indefinitely.
    pub async fn stopped(&self) {
        loop {
            if self.should_stop() {
                return;
            }
            let changed = self.state.changed.listen();
            if self.should_stop() {
                return;
            }
            changed.await;
        }
    }
}

impl std::fmt::Debug for StopToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StopToken")
            .field("requested", &self.should_stop())
            .finish()
    }
}
