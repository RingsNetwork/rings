//! Cooperative lifecycle primitives shared by native and browser runtimes.
//!
//! A [`StopSource`] is the authority that may request shutdown. A [`StopToken`]
//! is the read-only capability handed to long-running loops. The model is
//! intentionally monotonic: once a source requests stop, every token cloned from
//! that source observes stop forever.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_source_propagates_stop_to_existing_and_cloned_tokens() {
        let source = StopSource::new();
        let first = source.token();
        let second = first.clone();

        assert!(!first.should_stop());
        assert!(!second.should_stop());
        assert!(!source.is_stop_requested());

        source.request_stop();

        assert!(first.should_stop());
        assert!(second.should_stop());
        assert!(source.is_stop_requested());
    }

    #[test]
    fn cloned_stop_source_controls_the_same_lifecycle_scope() {
        let source = StopSource::new();
        let cloned_source = source.clone();
        let token = source.token();

        cloned_source.request_stop();

        assert!(token.should_stop());
        assert!(source.is_stop_requested());
    }

    #[test]
    fn never_token_is_independent_from_other_sources() {
        let source = StopSource::new();
        let token = StopToken::never();

        source.request_stop();

        assert!(!token.should_stop());
    }
}
