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
pub const UNBOUNDED_SLEEP: Duration = Duration::from_secs(4_294_967_295);

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

/// A duration decomposed into consecutive `setTimeout` delays.
///
/// `setTimeout` honours at most `i32::MAX` (`2³¹ − 1`) ms; a larger delay overflows and fires
/// at once, so a longer duration is emitted as several segments instead.
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
        // Total: a remainder beyond the ceiling saturates to it, so no segment is ever lost.
        let segment = i32::try_from(self.remaining_ms).unwrap_or(i32::MAX);
        // `0 ≤ segment ≤ remaining_ms`, so the subtraction cannot underflow.
        self.remaining_ms -= u128::from(segment.unsigned_abs());
        Some(segment)
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
mod browser {
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    use super::TimerError;
    use crate::global::global;
    use crate::global::Global;

    struct TimeoutShared {
        fired: Cell<bool>,
        waker: RefCell<Option<Waker>>,
    }

    /// A one-shot browser `setTimeout` future whose [`Drop`] cancels the host timer and frees its
    /// [`Closure`] whenever the wait ends before the timeout fires.
    struct BrowserTimeout {
        scope: Global,
        millis: i32,
        handle: Option<i32>,
        callback: Option<Closure<dyn FnMut()>>,
        shared: Rc<TimeoutShared>,
    }

    impl BrowserTimeout {
        fn new(millis: i32) -> Result<Self, TimerError> {
            let scope = global().ok_or(TimerError::NoGlobalScope)?;
            Ok(Self {
                scope,
                millis,
                handle: None,
                callback: None,
                shared: Rc::new(TimeoutShared {
                    fired: Cell::new(false),
                    waker: RefCell::new(None),
                }),
            })
        }
    }

    impl Future for BrowserTimeout {
        type Output = Result<(), TimerError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.shared.fired.get() {
                self.handle = None;
                self.callback = None;
                return Poll::Ready(Ok(()));
            }
            *self.shared.waker.borrow_mut() = Some(cx.waker().clone());
            if self.handle.is_none() {
                let shared = Rc::clone(&self.shared);
                let callback = Closure::wrap(Box::new(move || {
                    shared.fired.set(true);
                    let waker = shared.waker.borrow_mut().take();
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }) as Box<dyn FnMut()>);
                match self
                    .scope
                    .set_timeout_0(callback.as_ref().unchecked_ref(), self.millis)
                {
                    Ok(handle) => {
                        self.handle = Some(handle);
                        self.callback = Some(callback);
                    }
                    Err(error) => {
                        return Poll::Ready(Err(TimerError::Rejected(format!("{error:?}"))));
                    }
                }
            }
            Poll::Pending
        }
    }

    impl Drop for BrowserTimeout {
        fn drop(&mut self) {
            if !self.shared.fired.get() {
                if let Some(handle) = self.handle.take() {
                    self.scope.clear_timeout(handle);
                }
            }
            self.callback = None;
        }
    }

    /// Resolve after one `setTimeout` of `millis` on the current global scope.
    pub(super) async fn set_timeout(millis: i32) -> Result<(), TimerError> {
        BrowserTimeout::new(millis)?.await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::TimeoutSegments;

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
        let at_ceiling = Duration::from_millis(u64::from(max.unsigned_abs()));

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

    #[wasm_bindgen_test]
    async fn test_browser_dropped_sleep_cancels_scheduled_timeout() {
        use futures::FutureExt;

        for _ in 0..256 {
            let mut pending_sleep = Box::pin(sleep(Duration::from_secs(60)));
            assert!(pending_sleep.as_mut().now_or_never().is_none());
            drop(pending_sleep);
        }
        sleep(Duration::from_millis(20)).await.unwrap();
    }
}
