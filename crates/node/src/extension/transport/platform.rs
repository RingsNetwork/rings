//! Canonical runtime adapter for detached extension tasks.

use std::future::Future;

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
