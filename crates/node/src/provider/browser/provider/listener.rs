//! Serialized browser listener lifecycle.

use futures::channel::oneshot;
use rings_core::lifecycle::StopSource;
use rings_derive::wasm_export;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::provider::Provider;

/// Browser listener lifecycle handle returned by [`Provider::listen`].
#[derive(Clone)]
#[wasm_export]
pub struct ProviderListener {
    /// Cooperative shutdown source owned by this exported handle.
    stop: StopSource,
    /// Promise resolved after this listener generation acquires the provider gate.
    started: js_sys::Promise,
    /// Promise for the long-running listener task.
    task: js_sys::Promise,
}

#[wasm_export]
impl ProviderListener {
    /// Request cooperative shutdown for the listener task.
    pub fn stop(&self) {
        self.stop.request_stop();
    }

    /// Return whether shutdown was requested through this handle.
    pub fn is_stopped(&self) -> bool {
        self.stop.is_stop_requested()
    }

    /// Return a promise that resolves once the listener task enters its run loop.
    ///
    /// A queued listener does not resolve this promise until the previous
    /// generation has finished cleanup and released the provider gate.
    pub fn started(&self) -> js_sys::Promise {
        self.started.clone()
    }

    /// Return the underlying listener task promise.
    ///
    /// It resolves only after [`ProviderListener::stop`] requests cooperative shutdown.
    pub fn task(&self) -> js_sys::Promise {
        self.task.clone()
    }
}

#[wasm_export]
impl Provider {
    /// Start the long-running listener and return its lifecycle handle.
    ///
    /// Calls are serialized per provider instance. A new browser listener waits
    /// for the previous generation to finish its cooperative shutdown before it
    /// publishes `started`.
    pub fn listen(&self) -> ProviderListener {
        // Clone the processor before spawning the JS promise so the exported
        // Provider value can be dropped independently of the listener task.
        let processor = self.processor.clone();
        // Shared async mutex for all listener generations created from this provider.
        let listener_gate = self.listener_gate.clone();
        let stop = StopSource::new();
        // The token is moved into `listen_with`; the source stays in the handle
        // so JS callers can request shutdown later.
        let token = stop.token();
        // `started` resolves from this one-shot only after the task acquires
        // `listener_gate`, not merely when `listen()` returns.
        let (started_sender, started_receiver) = oneshot::channel::<()>();

        let started = future_to_promise(async move {
            started_receiver
                .await
                .map_err(|_| JsError::new("provider listener exited before start"))?;
            Ok(JsValue::null())
        });

        let task = future_to_promise(async move {
            // A stopped generation may still be completing IndexedDB or
            // transport work. Wait for it rather than duplicating daemons.
            let _listener_guard = listener_gate.lock().await;
            // Ignore receiver loss: dropping `started()` should not cancel the
            // listener task after it has acquired the gate.
            let _sent = started_sender.send(());
            processor.listen_with(token).await;
            Ok(JsValue::null())
        });

        ProviderListener {
            stop,
            started,
            task,
        }
    }

    #[cfg(test)]
    /// Return the browser listener serialization gate for lifecycle tests.
    pub(crate) fn listener_gate_for_test(&self) -> std::sync::Arc<futures::lock::Mutex<()>> {
        self.listener_gate.clone()
    }
}
