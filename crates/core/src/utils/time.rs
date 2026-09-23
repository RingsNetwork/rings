#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use chrono::Utc;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) use tokio::time::Instant;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) use web_time::Instant;

/// Get local utc timestamp (millisecond)
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub fn get_epoch_ms() -> u128 {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if let Some(now_ms) = crate::simulation::epoch_ms_override() {
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
    match rings_runtime::sleep(duration).await {
        Ok(()) => true,
        Err(error) => {
            tracing::error!("failed to wait for timeout: {error}");
            false
        }
    }
}
