//! Event witness for tests that must await a state, never a duration.

use tokio::sync::watch;

/// Monotonic generation of one observed state.
///
/// Law: `bump` runs only after the observed state is fully applied and after
/// every lock over that state is released. A waiter therefore re-checks its
/// predicate against the applied state, and the lock order `witness -> state`
/// inside [`Self::await_until`] cannot invert against a writer that holds the
/// state lock while it bumps. `watch` retains the latest generation, so a bump
/// that races the waiter's registration is never lost; a state that never
/// arrives surfaces as a hang bounded only by the test harness, never as a
/// sub-second wall-clock flake.
pub(crate) struct GenerationWitness(watch::Sender<u64>);

impl GenerationWitness {
    /// Start at generation zero.
    pub(crate) fn new() -> Self {
        Self(watch::Sender::new(0))
    }

    /// Publish that the observed state advanced.
    ///
    /// Pre: the state change is visible and its lock is released.
    pub(crate) fn bump(&self) {
        self.0.send_modify(|generation| *generation += 1);
    }

    /// Current generation.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn generation(&self) -> u64 {
        *self.0.borrow()
    }

    /// Resolve once `predicate` holds, evaluating it now and after every bump.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_until(&self, predicate: impl Fn() -> bool) {
        let mut receiver = self.0.subscribe();
        // The sender is owned by the witnessed state and outlives every
        // waiter, so the closed-channel error cannot occur; a parked waiter
        // simply resolves on the next bump.
        let _ = receiver.wait_for(|_generation| predicate()).await;
    }
}

impl Default for GenerationWitness {
    fn default() -> Self {
        Self::new()
    }
}
