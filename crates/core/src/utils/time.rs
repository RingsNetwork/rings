use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use chrono::Utc;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) use tokio::time::Instant;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) use web_time::Instant;

/// Capability for reading the current Unix epoch timestamp in milliseconds.
///
/// Protocol handlers and session drivers read time through an injected `Clock` rather than
/// consulting ambient wall-clock globals directly, allowing deterministic replay and sleep-free
/// unit testing via [`ManualClock`].
pub trait Clock: Send + Sync + fmt::Debug {
    /// Return the current Unix epoch timestamp in milliseconds.
    fn now_ms(&self) -> u128;

    /// Return the current Unix epoch timestamp in milliseconds, saturating at `i64::MAX`.
    fn now_ms_i64(&self) -> i64 {
        i64::try_from(self.now_ms()).unwrap_or(i64::MAX)
    }
}

/// Shared, cloneable handle to a [`Clock`] capability.
pub type SharedClock = Arc<dyn Clock>;

/// Production [`Clock`] reading the platform wall clock (or active simulation override in `dummy`
/// test builds).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

/// Static [`SystemClock`] instance for zero-allocation default [`Clock`] borrows.
pub(crate) const SYSTEM_CLOCK: SystemClock = SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u128 {
        get_epoch_ms()
    }
}

/// Deterministic, caller-driven [`Clock`] for unit tests and trace replay.
#[derive(Clone, Debug)]
pub struct ManualClock {
    now_ms: Arc<Mutex<u128>>,
}

impl ManualClock {
    /// Construct a manual clock initialized to `initial_ms`.
    pub fn new(initial_ms: u128) -> Self {
        Self {
            now_ms: Arc::new(Mutex::new(initial_ms)),
        }
    }

    /// Wrap an existing shared millisecond cell.
    pub fn from_shared(now_ms: Arc<Mutex<u128>>) -> Self {
        Self { now_ms }
    }

    /// Return the underlying shared millisecond cell.
    pub fn shared(&self) -> Arc<Mutex<u128>> {
        Arc::clone(&self.now_ms)
    }

    /// Set the clock's current timestamp to `now_ms`.
    pub fn set(&self, now_ms: u128) {
        *self.now_ms.lock().unwrap_or_else(PoisonError::into_inner) = now_ms;
    }

    /// Advance the clock by `delta_ms` milliseconds, saturating on `u128` overflow, and return the
    /// updated timestamp.
    pub fn advance(&self, delta_ms: u128) -> u128 {
        let mut guard = self.now_ms.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = guard.saturating_add(delta_ms);
        *guard
    }

    /// Advance the clock by `delta_ms` milliseconds, returning `None` on overflow.
    pub fn checked_advance(&self, delta_ms: u128) -> Option<u128> {
        let mut guard = self.now_ms.lock().unwrap_or_else(PoisonError::into_inner);
        let next = guard.checked_add(delta_ms)?;
        *guard = next;
        Some(next)
    }

    /// Advance the clock by `duration`, saturating on overflow, and return the updated timestamp.
    pub fn advance_duration(&self, duration: Duration) -> u128 {
        self.advance(duration.as_millis())
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u128 {
        *self.now_ms.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Get local utc timestamp (millisecond)
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub fn get_epoch_ms() -> u128 {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if let Some(now_ms) = crate::simulation::epoch_ms_override() {
        return now_ms;
    }
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    if let Some(now_ms) = crate::dummy_simulation::epoch_ms_override() {
        return now_ms;
    }
    Utc::now().timestamp_millis() as u128
}

/// Get local utc timestamp (millisecond)
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub fn get_epoch_ms() -> u128 {
    let now = js_sys::Date::now();
    if now.is_finite() && now > 0.0 {
        now as u128
    } else {
        0
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) fn get_epoch_ms_i64() -> i64 {
    i64::try_from(get_epoch_ms()).unwrap_or(i64::MAX)
}

/// Sleep for `duration`; a timer the runtime cannot run ends the wait early.
///
/// One-shot timeout users fail closed on an early end. Repeating loops must call
/// [`try_sleep`] and stop explicitly, so a rejected timer cannot become a hot retry loop.
pub(crate) async fn sleep(duration: std::time::Duration) {
    let _ = try_sleep(duration).await;
}

/// Wait for `duration`. Post: `false` iff the runtime could not run the timer.
///
/// Deterministic dummy simulations follow Tokio's paused clock; every other caller waits on
/// the shared [`rings_runtime::sleep`] contract.
pub(crate) async fn try_sleep(duration: std::time::Duration) -> bool {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if crate::simulation::epoch_ms_override().is_some() {
        tokio::time::sleep(duration).await;
        return true;
    }
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    if crate::dummy_simulation::epoch_ms_override().is_some() {
        tokio::time::sleep(duration).await;
        return true;
    }
    match rings_runtime::sleep(duration).await {
        Ok(()) => true,
        Err(error) => {
            tracing::error!("failed to wait for timeout: {error}");
            false
        }
    }
}
