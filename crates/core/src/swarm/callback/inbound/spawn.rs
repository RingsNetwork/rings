//! Runtime-specific inbound actor spawning boundary.

use super::InboundActor;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) fn spawn_actor(actor: InboundActor) -> bool {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    runtime.spawn(actor.run());
    true
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) fn spawn_actor(actor: InboundActor) -> bool {
    wasm_bindgen_futures::spawn_local(actor.run());
    true
}
