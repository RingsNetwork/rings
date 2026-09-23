//! Serialized browser listener lifecycle.
//!
//! The shared [`Processor`] owns the asynchronous listener lifecycle lock. A
//! browser generation does not announce that it has started until it owns the lock,
//! and it retains ownership through cooperative shutdown and measurement flush.
//!
//! # Algorithm flow
//!
//! ```text
//! Provider::listen
//!       |
//!       +--> create stop source and one-shot start signal
//!       |
//!       +--> return ProviderListener with two JavaScript promises
//!                    |
//!                    v
//!            task waits for processor lifecycle lock
//!                    |
//!                    v
//!            Processor::listen_with_started acquires ownership
//!                    |
//!                    +--> resolve started promise
//!                    |
//!                    v
//!            run processor.listen_with(stop token)
//!                    |
//!          stop() requests cooperative shutdown
//!                    |
//!                    v
//!            listener cleanup completes
//!                    |
//!                    v
//!            release processor lifecycle lock and resolve task promise
//! ```

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
    /// Cooperative shutdown authority retained by the exported handle.
    ///
    /// Calling [`Self::stop`] flips the paired token observed by
    /// `Processor::listen_with`; dropping this source alone does not report a
    /// successful listener shutdown to JavaScript.
    stop: StopSource,
    /// Promise resolved after this generation acquires the processor lifecycle lock.
    ///
    /// It deliberately remains pending while an earlier generation is still
    /// cleaning up, so callers never confuse task creation with active service.
    started: js_sys::Promise,
    /// Promise representing the complete long-running listener generation.
    ///
    /// Resolution means `listen_with` returned and released the lifecycle lock;
    /// callers may then start another generation without overlap.
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
    /// generation has finished cleanup and released the processor lifecycle lock.
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
    /// Calls queue on the shared processor, including starts through provider
    /// clones or other wrappers. A new browser listener publishes `started` only
    /// after the previous generation finishes cooperative shutdown and flushing.
    pub fn listen(&self) -> ProviderListener {
        // Clone the processor before spawning the JS promise so the exported
        // Provider value can be dropped independently of the listener task.
        let processor = self.processor.clone();
        let stop = StopSource::new();
        // The token is moved into `listen_with`; the source stays in the handle
        // so JS callers can request shutdown later.
        let token = stop.token();
        // `started` resolves only after the processor grants this generation ownership.
        let (started_sender, started_receiver) = oneshot::channel::<()>();

        let started = future_to_promise(async move {
            started_receiver
                .await
                .map_err(|_| JsError::new("provider listener exited before start"))?;
            Ok(JsValue::null())
        });

        let task = future_to_promise(async move {
            processor
                .listen_with_started(token, || {
                    // Ignore receiver loss: dropping `started()` must not cancel
                    // the listener after it has acquired processor ownership.
                    let _sent = started_sender.send(());
                })
                .await;
            Ok(JsValue::null())
        });

        ProviderListener {
            stop,
            started,
            task,
        }
    }

    #[cfg(test)]
    /// Return the processor-owned listener lifecycle lock for lifecycle tests.
    pub(crate) fn listener_lifecycle_lock_for_test(
        &self,
    ) -> std::sync::Arc<futures::lock::Mutex<()>> {
        self.processor.listener_lifecycle_lock_for_test()
    }
}
