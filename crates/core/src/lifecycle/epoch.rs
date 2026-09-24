//! A monotone event counter that waiters can await without a duration.
//!
//! An [`Epoch`] counts the occurrences of one class of event (a topology commit, a link
//! transition, a capacity release). Its value only grows, so a reading `e₀` taken before a
//! computation is a *stamp*: `current() > e₀` holds iff an event of the class happened after the
//! stamp, and no event can be lost between the stamp and a later wait.
//!
//! ```text
//! Epoch      ≜ (value : ℕ, changed : Event)
//! advance    : value' = value + 1 ∧ notify(changed, all)
//! Law (Mono) : □(value' ≥ value)
//! Law (Wake) : a listener registered at value v is notified by every advance from v,
//!              so  listen ; check(predicate) ; await   misses no advance.
//! ```

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use event_listener::Event;
use event_listener::EventListener;

/// A monotone count of one class of events, with a notification per advance.
///
/// Inv: `value` never decreases; every [`Self::advance`] notifies every listener registered
/// before it.
#[derive(Debug, Default)]
pub(crate) struct Epoch {
    /// Number of events of this class so far.
    value: AtomicU64,
    /// Notified after every increment of `value`.
    changed: Event,
}

impl Epoch {
    /// Record one event: increment the count, then wake every listener.
    ///
    /// Post: `current()` observed after this call exceeds every reading taken before it (one
    /// process cannot perform the `2^64` increments that would wrap the count).
    pub(crate) fn advance(&self) {
        self.value.fetch_add(1, Ordering::AcqRel);
        self.changed.notify(usize::MAX);
    }

    /// The number of events recorded so far: a stamp for [`Self::listen`] predicates.
    pub(crate) fn current(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }

    /// Register a listener for the next advance.
    ///
    /// Pre: the caller evaluates its predicate *after* this call and awaits the listener only
    /// when the predicate is false; `Law (Wake)` then rules out a lost notification.
    pub(crate) fn listen(&self) -> EventListener {
        self.changed.listen()
    }

    /// Test hook: the listeners registered and not yet notified or dropped.
    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn listeners(&self) -> usize {
        self.changed.total_listeners()
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    use super::Epoch;

    /// Law (Mono): every advance increases the count by one.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_every_advance_increases_the_count() {
        let epoch = Epoch::default();
        let stamp = epoch.current();
        epoch.advance();
        epoch.advance();
        assert_eq!(epoch.current(), stamp + 2);
    }

    /// Law (Wake): a listener registered before an advance is notified by it; one registered
    /// after it waits for the next advance.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_a_listener_is_notified_by_every_later_advance() {
        let epoch = Epoch::default();
        let registered_before = epoch.listen();
        epoch.advance();
        let registered_after = epoch.listen();
        assert!(registered_before.now_or_never().is_some());
        assert!(registered_after.now_or_never().is_none());
    }
}
