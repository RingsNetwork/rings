//! The one boundary at which the swarm hands a task to the runtime to run on its own: the
//! inbound actor, the pre-admission drain, the session-hold release, and every link-control
//! send go through it. Native needs a current tokio runtime; the browser always has one.
//!
//! A task is boxed at the boundary: a task that can start another of its own kind (a release
//! that admits a frame that could start a release) has a future of recursive type, and the
//! box is what makes it finite.

use std::future::Future;
use std::pin::Pin;

/// A task ready to run on its own.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) type DetachedTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A task ready to run on its own; the browser runtime is single-threaded.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) type DetachedTask = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// Run `task` detached from the caller. Post: `Ok` means the runtime took it; `Err` hands
/// the task back because no runtime is current, and the caller runs it inline or refuses.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) fn spawn_detached(task: DetachedTask) -> Result<(), DetachedTask> {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            drop(runtime.spawn(task));
            Ok(())
        }
        Err(_) => Err(task),
    }
}

/// Run `task` detached from the caller; the browser runtime always can.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) fn spawn_detached(task: DetachedTask) -> Result<(), DetachedTask> {
    wasm_bindgen_futures::spawn_local(task);
    Ok(())
}
