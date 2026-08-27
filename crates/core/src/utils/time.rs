#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use chrono::Utc;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
use super::js_utils;

/// Get local utc timestamp (millisecond)
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub fn get_epoch_ms() -> u128 {
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

/// Sleep for `duration` on the active runtime.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) async fn sleep(duration: std::time::Duration) {
    futures_timer::Delay::new(duration).await;
}

/// Sleep for `duration` on the JavaScript event loop.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) async fn sleep(duration: std::time::Duration) {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    if let Err(error) = js_utils::window_sleep(millis).await {
        tracing::error!("failed to wait for timeout: {:?}", error);
    }
}
