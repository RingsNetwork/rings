use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) use std::time::Instant as OsInstant;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::time::SystemTime;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::time::UNIX_EPOCH;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) use tokio::time::Instant;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) use web_time::Instant;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) use web_time::Instant as OsInstant;

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

    /// Whether this clock tracks real platform wall time (`true` for [`SystemClock`], `false`
    /// for deterministic test clocks such as [`ManualClock`]).
    ///
    /// [`crate::dht::PeerRing::with_clock`] uses this to preserve monotonic read-clock fencing on
    /// production wall clocks without shifting deterministic test clocks by host CPU elapsed time
    /// (#1332).
    fn is_wall_clock(&self) -> bool {
        false
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

    fn is_wall_clock(&self) -> bool {
        true
    }
}

/// Deterministic, caller-driven [`Clock`] for unit tests and trace replay.
#[derive(Clone, Debug)]
pub struct ManualClock {
    /// Shared cell holding the current simulated timestamp in milliseconds.
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

/// Pure read-clock floor combining monotonic elapsed time with Byzantine-safe admitted HLC
/// observations (#1332).
///
/// Prevents backward wall-clock steps, awake wall-clock stalls while the monotonic clock advances,
/// and rollback below `τ - σ` for any admitted HLC version (`τ ≤ t_admit + σ ⇒ τ - σ ≤ t_admit`).
/// If OS/browser suspension pauses both wall and monotonic clocks prior to NTP resynchronization,
/// no single untrusted peer can safely advance the read floor past local wall time without
/// breaking Byzantine retirement safety, so this floor complements OS/NTP resynchronization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClockFloor {
    /// Last `(wall_ms, monotonic_ms)` anchor pair used to project monotonic elapsed time.
    anchor: Option<(u128, u128)>,
    /// Highest Byzantine-safe lower bound derived from admitted HLC timestamps.
    observed_hlc_floor_ms: u128,
}

impl ClockFloor {
    /// Construct an unanchored zero floor.
    pub const fn new() -> Self {
        Self {
            anchor: None,
            observed_hlc_floor_ms: 0,
        }
    }

    /// Project the anchored wall-clock estimate forward to `monotonic_ms`, if anchored.
    fn monotonic_floor_ms(&self, monotonic_ms: u128) -> Option<u128> {
        self.anchor.map(|(anchor_wall_ms, anchor_mono_ms)| {
            anchor_wall_ms.saturating_add(monotonic_ms.saturating_sub(anchor_mono_ms))
        })
    }

    /// Advance the anchor when `wall_ms` progresses past the monotonic estimate and return the
    /// effective guarded timestamp `max(wall_ms, monotonic_floor, observed_hlc_floor)`.
    pub fn observe_wall(&mut self, wall_ms: u128, monotonic_ms: u128) -> u128 {
        let monotonic_floor = self.monotonic_floor_ms(monotonic_ms).unwrap_or(wall_ms);
        if self.anchor.is_none() || wall_ms >= monotonic_floor {
            self.anchor = Some((wall_ms, monotonic_ms));
        }
        wall_ms.max(monotonic_floor).max(self.observed_hlc_floor_ms)
    }

    /// Record an already-admitted HLC timestamp `hlc_ms` with skew tolerance `tolerance_ms`.
    ///
    /// Subtracting `tolerance_ms` guarantees an adversarial sender carrying up to `now + σ` cannot
    /// push this local read floor ahead of the true admission instant.
    pub fn observe_admitted_hlc(&mut self, hlc_ms: u128, tolerance_ms: u128) -> u128 {
        let floored = hlc_ms.saturating_sub(tolerance_ms);
        self.observed_hlc_floor_ms = self.observed_hlc_floor_ms.max(floored);
        self.observed_hlc_floor_ms
    }

    /// Return the Byzantine-safe HLC lower bound recorded so far.
    pub const fn observed_hlc_floor_ms(&self) -> u128 {
        self.observed_hlc_floor_ms
    }

    /// Return the current monotonic/HLC lower bound at `monotonic_ms` without re-anchoring.
    pub fn floor_ms(&self, monotonic_ms: u128) -> u128 {
        self.monotonic_floor_ms(monotonic_ms)
            .unwrap_or(0)
            .max(self.observed_hlc_floor_ms)
    }

    /// Return how far `wall_ms` lags behind [`Self::floor_ms`] at `monotonic_ms`.
    pub fn lag_ms(&self, wall_ms: u128, monotonic_ms: u128) -> u128 {
        self.floor_ms(monotonic_ms).saturating_sub(wall_ms)
    }
}

/// Return whether [`get_epoch_ms`] is currently overridden by a deterministic simulation clock.
pub(crate) fn is_simulated_epoch_ms() -> bool {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if crate::simulation::epoch_ms_override().is_some() {
        return true;
    }
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    if crate::dummy_simulation::epoch_ms_override().is_some() {
        return true;
    }
    false
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::TS_OFFSET_TOLERANCE_MS;

    #[test]
    fn clock_floor_guards_against_lagging_and_backward_wall_clock() {
        let mut floor = ClockFloor::new();
        assert_eq!(
            floor.observe_wall(1_700_000_000_000, 100),
            1_700_000_000_000
        );
        // Simulate 30s sleep on monotonic clock while wall clock only moved 2s.
        let guarded = floor.observe_wall(1_700_000_002_000, 30_100);
        assert_eq!(guarded, 1_700_000_030_000);
        assert_eq!(floor.lag_ms(1_700_000_002_000, 30_100), 28_000);

        // Once NTP catches up past monotonic time, wall clock re-anchors forward.
        let caught_up = floor.observe_wall(1_700_000_040_000, 31_100);
        assert_eq!(caught_up, 1_700_000_040_000);
        assert_eq!(floor.lag_ms(1_700_000_040_000, 31_100), 0);

        // Backward wall-clock jump stays guarded by monotonic elapsed time.
        let backward = floor.observe_wall(1_700_000_010_000, 32_100);
        assert_eq!(backward, 1_700_000_041_000);
    }

    #[test]
    fn clock_floor_admitted_hlc_respects_tolerance_bound() {
        let mut floor = ClockFloor::new();
        let admit_t = 1_700_000_000_000;
        // Peer carrying the maximum skew τ = t + σ cannot advance floor above t.
        assert_eq!(
            floor.observe_admitted_hlc(admit_t + TS_OFFSET_TOLERANCE_MS, TS_OFFSET_TOLERANCE_MS),
            admit_t
        );
        // Lagging local clock 10s behind `admit_t` is floored to `admit_t`.
        let floored = floor.observe_admitted_hlc(
            admit_t + 5_000 + TS_OFFSET_TOLERANCE_MS,
            TS_OFFSET_TOLERANCE_MS,
        );
        assert_eq!(floored, admit_t + 5_000);
        assert_eq!(floor.observe_wall(admit_t - 10_000, 0), admit_t + 5_000);

        assert!(SystemClock.is_wall_clock());
        assert!(!ManualClock::new(0).is_wall_clock());
    }
}
