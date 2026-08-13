//! Canonical runtime adapter for detached extension tasks.

use std::future::Future;
use std::time::Duration;

use crate::error::Result;

/// Spawn owned extension work without tying its completion to the current protocol turn.
///
/// Native tasks must be `Send`; browser tasks remain on the single-threaded wasm executor.
#[cfg(rings_native)]
pub(crate) fn spawn_detached<F>(future: F)
where F: Future<Output = ()> + Send + 'static {
    tokio::spawn(future);
}

/// Browser counterpart of [`spawn_detached`].
#[cfg(rings_browser)]
pub(crate) fn spawn_detached<F>(future: F)
where F: Future<Output = ()> + 'static {
    wasm_bindgen_futures::spawn_local(future);
}

/// Wait without exposing a target-specific timer to protocol interpreters.
#[cfg(rings_native)]
pub(crate) async fn sleep(duration: Duration) -> Result<()> {
    futures_timer::Delay::new(duration).await;
    Ok(())
}

/// Browser and worker counterpart of [`sleep`].
#[cfg(rings_browser)]
pub(crate) async fn sleep(duration: Duration) -> Result<()> {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    rings_core::utils::js_utils::window_sleep(millis)
        .await
        .map(|_| ())
        .map_err(|error| crate::error::Error::JsError(format!("{error:?}")))
}
