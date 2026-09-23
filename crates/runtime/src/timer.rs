//! The one timer Rings waits on.
//!
//! ```text
//!   sleep : Duration → Future (Result<(), TimerError>)
//! ```
//!
//! Laws, identical on both targets:
//!
//! * **Lower bound.** `sleep(d)` never resolves `Ok` before `d` has elapsed (up to the host
//!   clock's resolution). The browser rounds `d` *up* to whole milliseconds and chains
//!   `setTimeout` calls across its `2³¹ − 1` ms ceiling instead of clamping, which would
//!   fire early.
//! * **Unbounded.** `sleep(d)` with `d ≥` [`UNBOUNDED_SLEEP`] never resolves: a deadline that
//!   far out is "never", not an instant the clock would overflow computing.
//! * **Failure.** Only the browser can fail — when no global scope owns `setTimeout`, or the
//!   host rejects the timer — and it does so promptly with `Err`. A repeating caller must
//!   stop on `Err`: retrying would turn a missing timer into a hot loop.
//!
//! Native sleeps use `futures-timer`, which needs no particular executor; code that must
//! follow a controlled test clock (Tokio's paused time) keeps that override at its own
//! boundary.

use std::time::Duration;

/// Durations at or beyond this bound (`2³² − 1` s, about 136 years) sleep forever.
pub const UNBOUNDED_SLEEP: Duration = Duration::from_secs(u32::MAX as u64);

/// Why the browser could not wait.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TimerError {
    /// The code runs outside a window, worker or service-worker global scope.
    #[error("no JavaScript global scope owns setTimeout")]
    NoGlobalScope,
    /// The host refused to schedule the timeout.
    #[error("JavaScript timer rejected: {0}")]
    Rejected(String),
}

/// Wait for `duration` on the native timer thread; never fails.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub async fn sleep(duration: Duration) -> Result<(), TimerError> {
    if duration >= UNBOUNDED_SLEEP {
        return std::future::pending().await;
    }
    futures_timer::Delay::new(duration).await;
    Ok(())
}

/// Wait for `duration` on the JavaScript event loop.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub async fn sleep(duration: Duration) -> Result<(), TimerError> {
    if duration >= UNBOUNDED_SLEEP {
        return std::future::pending().await;
    }
    for millis in TimeoutSegments::new(duration) {
        browser::set_timeout(millis).await?;
    }
    Ok(())
}

/// The largest delay `setTimeout` honours; larger values overflow and fire immediately.
#[cfg(any(test, all(feature = "browser", target_family = "wasm")))]
const MAX_TIMEOUT_MS: u128 = i32::MAX as u128;

/// A duration decomposed into consecutive `setTimeout` delays.
///
/// Laws: every segment lies in `0 ..= 2³¹ − 1`; the segments sum to `⌈d⌉` in milliseconds;
/// a zero duration is one zero segment, so `sleep(0)` still yields to the event loop.
#[cfg(any(test, all(feature = "browser", target_family = "wasm")))]
struct TimeoutSegments {
    /// Milliseconds not yet covered by an emitted segment.
    remaining_ms: u128,
    /// Whether no segment has been emitted yet.
    fresh: bool,
}

#[cfg(any(test, all(feature = "browser", target_family = "wasm")))]
impl TimeoutSegments {
    /// Segments covering `duration`, rounded up to whole milliseconds.
    fn new(duration: Duration) -> Self {
        Self {
            remaining_ms: duration.as_nanos().div_ceil(1_000_000),
            fresh: true,
        }
    }
}

#[cfg(any(test, all(feature = "browser", target_family = "wasm")))]
impl Iterator for TimeoutSegments {
    type Item = i32;

    fn next(&mut self) -> Option<i32> {
        if self.remaining_ms == 0 && !self.fresh {
            return None;
        }
        self.fresh = false;
        let segment = self.remaining_ms.min(MAX_TIMEOUT_MS);
        self.remaining_ms -= segment;
        i32::try_from(segment).ok()
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
mod browser {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    use super::TimerError;
    use crate::global::global;

    /// Resolve after one `setTimeout` of `millis` on the current global scope.
    pub(super) async fn set_timeout(millis: i32) -> Result<(), TimerError> {
        let scope = global().ok_or(TimerError::NoGlobalScope)?;
        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let wake = Closure::once_into_js(move || {
                // Resolving a fresh promise from its own executor cannot throw.
                let _ = resolve.call0(&JsValue::NULL);
            });
            if let Err(error) = scope.set_timeout_0(wake.unchecked_ref(), millis) {
                let _ = reject.call1(&JsValue::NULL, &error);
            }
        });
        wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .map(drop)
            .map_err(|error| TimerError::Rejected(format!("{error:?}")))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::TimeoutSegments;
    use super::MAX_TIMEOUT_MS;

    fn segments(duration: Duration) -> Vec<i32> {
        TimeoutSegments::new(duration).collect()
    }

    #[test]
    fn test_zero_duration_still_yields_once() {
        assert_eq!(segments(Duration::ZERO), vec![0]);
    }

    #[test]
    fn test_sub_millisecond_rounds_up() {
        assert_eq!(segments(Duration::from_nanos(1)), vec![1]);
        assert_eq!(segments(Duration::from_micros(1_500)), vec![2]);
        assert_eq!(segments(Duration::from_millis(7)), vec![7]);
    }

    #[test]
    fn test_ceiling_is_chained_not_clamped() {
        let max = i32::MAX;
        let at_ceiling = Duration::from_millis(u64::try_from(MAX_TIMEOUT_MS).unwrap());

        assert_eq!(segments(at_ceiling), vec![max]);
        assert_eq!(segments(at_ceiling + Duration::from_millis(1)), vec![
            max, 1
        ]);
        assert_eq!(segments(at_ceiling * 2), vec![max, max]);
    }

    #[test]
    fn test_segments_sum_to_the_rounded_duration() {
        for nanos in [0_u64, 1, 999_999, 1_000_000, 86_400_000_000_000, u64::MAX] {
            let duration = Duration::from_nanos(nanos);
            let emitted = segments(duration);
            let total: u128 = emitted
                .iter()
                .map(|&ms| u128::from(ms.unsigned_abs()))
                .sum();

            assert_eq!(total, duration.as_nanos().div_ceil(1_000_000));
            assert!(emitted.iter().all(|&ms| ms >= 0));
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod native_tests {
    use std::time::Duration;

    use futures::FutureExt;

    use super::sleep;
    use super::UNBOUNDED_SLEEP;

    #[test]
    fn test_unbounded_sleep_never_resolves_or_overflows() {
        let mut never = Box::pin(sleep(Duration::MAX));
        assert!(never.as_mut().now_or_never().is_none());
        assert!(Box::pin(sleep(UNBOUNDED_SLEEP)).now_or_never().is_none());
    }

    #[test]
    fn test_native_sleep_never_fails() {
        assert_eq!(futures::executor::block_on(sleep(Duration::ZERO)), Ok(()));
    }
}

#[cfg(all(test, feature = "browser", target_family = "wasm"))]
mod browser_tests {
    use std::time::Duration;

    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::sleep;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn test_browser_sleep_resolves_after_its_duration() {
        let started = js_sys::Date::now();
        sleep(Duration::from_millis(20)).await.unwrap();

        // `Date.now()` is quantised to whole (possibly coarsened) milliseconds.
        assert!(js_sys::Date::now() - started >= 19.0);
    }

    #[wasm_bindgen_test]
    async fn test_browser_zero_sleep_yields_and_resolves() {
        sleep(Duration::ZERO).await.unwrap();
    }
}
