//! Event witness for tests that must await a state, never a duration.
//!
//! A [`Witness`] publishes one observed value. Production builds carry a
//! zero-sized witness whose publications are no-ops, so the types that own a
//! witness need no conditional fields or constructor parameters; test builds
//! back it with a `watch` channel that retains the latest value.
//!
//! Law: a publication runs only after the observed state is fully applied,
//! and the publisher holds no lock that a waiter's predicate acquires. A
//! waiter therefore re-checks its predicate against the applied state, the
//! lock order `witness -> state` inside [`Witness::await_until`] cannot
//! invert, and a publication that races the waiter's registration is never
//! lost. A state that never arrives surfaces as a hang bounded only by the
//! test harness, never as a sub-second wall-clock flake.

#[cfg(all(test, not(target_family = "wasm")))]
use tokio::sync::watch;

/// One observed value published to waiting tests.
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) struct Witness<T>(watch::Sender<T>);

/// One observed value; production builds publish nothing.
#[cfg(not(all(test, not(target_family = "wasm"))))]
pub(crate) struct Witness<T>(core::marker::PhantomData<T>);

/// Monotonic generation of a state whose value lives elsewhere.
pub(crate) type GenerationWitness = Witness<u64>;

#[cfg(all(test, not(target_family = "wasm")))]
impl<T> Witness<T> {
    /// Start at `initial`.
    pub(crate) fn new(initial: T) -> Self {
        Self(watch::Sender::new(initial))
    }

    /// Publish an update to the observed value.
    ///
    /// Pre: the state change it describes is visible and its lock is released.
    pub(crate) fn modify(&self, update: impl FnOnce(&mut T)) {
        self.0.send_modify(update);
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl<T: Copy> Witness<T> {
    /// Current value.
    pub(crate) fn get(&self) -> T {
        *self.0.borrow()
    }

    /// Resolve once `predicate` holds for the published value, evaluating it
    /// now and after every publication. The value is handed to the predicate,
    /// so the predicate must not read the witness again while it runs.
    pub(crate) async fn await_until(&self, predicate: impl Fn(T) -> bool) {
        let mut receiver = self.0.subscribe();
        // The sender is owned by the witnessed state and outlives every
        // waiter, so the closed-channel error cannot occur; a parked waiter
        // simply resolves on the next publication.
        let _ = receiver.wait_for(|value| predicate(*value)).await;
    }
}

#[cfg(not(all(test, not(target_family = "wasm"))))]
impl<T> Witness<T> {
    /// Start at `initial`, which production builds discard.
    pub(crate) fn new(_initial: T) -> Self {
        Self(core::marker::PhantomData)
    }

    /// Production builds publish nothing.
    pub(crate) fn modify(&self, _update: impl FnOnce(&mut T)) {}
}

impl Witness<u64> {
    /// Publish that the observed state advanced by one generation.
    pub(crate) fn bump(&self) {
        self.modify(|generation| *generation += 1);
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl Witness<bool> {
    /// Latch the event; idempotent.
    pub(crate) fn set(&self) {
        self.modify(|set| *set = true);
    }

    /// Whether the event has been latched.
    pub(crate) fn is_set(&self) -> bool {
        self.get()
    }

    /// Resolve once the event is latched.
    pub(crate) async fn wait(&self) {
        self.await_until(|set| set).await;
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl Witness<usize> {
    /// Increment and return the previous value.
    pub(crate) fn increment(&self) -> usize {
        let mut previous = 0;
        self.modify(|count| {
            previous = *count;
            *count += 1;
        });
        previous
    }
}

impl<T: Default> Default for Witness<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::*;

    /// Law: a publication before the waiter subscribes is retained.
    #[tokio::test]
    async fn test_publication_before_subscription_is_observed() {
        let witness = GenerationWitness::new(0);
        witness.bump();

        witness.await_until(|generation| generation >= 1).await;

        assert_eq!(witness.get(), 1);
    }

    /// Law: the predicate is re-evaluated after every publication until it
    /// holds, never on a clock.
    #[tokio::test]
    async fn test_predicate_is_reevaluated_on_each_publication() {
        let witness = Arc::new(GenerationWitness::new(0));
        let evaluations = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let witness = Arc::clone(&witness);
            let evaluations = Arc::clone(&evaluations);
            tokio::spawn(async move {
                witness
                    .await_until(|generation| {
                        evaluations.fetch_add(1, Ordering::SeqCst);
                        generation >= 3
                    })
                    .await;
            })
        };
        for _ in 0..3 {
            witness.bump();
            tokio::task::yield_now().await;
        }

        waiter
            .await
            .expect("waiter must resolve after the third bump");

        assert_eq!(witness.get(), 3);
        assert!(evaluations.load(Ordering::SeqCst) >= 2);
    }

    /// Law: a latch set once resolves every later wait without a wake-up.
    #[tokio::test]
    async fn test_latch_set_is_retained_for_late_waiters() {
        let latch = Witness::<bool>::default();
        assert!(!latch.is_set());
        latch.set();
        latch.set();

        latch.wait().await;

        assert!(latch.is_set());
    }
}
