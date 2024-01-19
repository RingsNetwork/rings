//! This module contains the [Notifier] struct.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

#[derive(Default)]
struct NotifierState {
    /// Indicates whether state has woken.
    pub(crate) woken: bool,

    /// The wakers associated with State.
    pub(crate) wakers: Vec<std::task::Waker>,
}

/// A notifier that can be woken by calling `wake` or `set_timeout`.
/// Used to notify the data channel state changing in `webrtc_wait_for_data_channel_open` of
/// [crate::core::transport::ConnectionInterface].
#[derive(Clone, Default)]
pub struct Notifier(Arc<Mutex<NotifierState>>);

impl Notifier {
    /// Immediately wake the notifier.
    pub fn wake(&self) {
        let mut state = self.0.lock().unwrap();
        state.woken = true;
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
    }

    /// Wake the notifier after the specified time.
    #[cfg(not(feature = "web-sys-webrtc"))]
    pub fn set_timeout(&self, seconds: u8) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(seconds.into())).await;
            this.wake();
        });
    }

    /// Wake the notifier after the specified time.
    #[cfg(feature = "web-sys-webrtc")]
    pub fn set_timeout(&self, seconds: u8) {
        use wasm_bindgen::JsCast;

        let millis = seconds as i32 * 1000;

        let this = self.clone();
        let wake = wasm_bindgen::closure::Closure::once_into_js(move || {
            this.wake();
        });

        match js_utils::global().unwrap() {
            js_utils::Global::Window(window) => window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    wake.as_ref().unchecked_ref(),
                    millis,
                ),
            js_utils::Global::WorkerGlobal(window) => window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    wake.as_ref().unchecked_ref(),
                    millis,
                ),
            js_utils::Global::ServiceWorkerGlobal(window) => window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    wake.as_ref().unchecked_ref(),
                    millis,
                ),
        }
        .unwrap();
    }
}

impl Future for Notifier {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap();

        if state.woken {
            return Poll::Ready(());
        }

        state.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

// This is copied from utils module of rings-core crate.
#[cfg(feature = "web-sys-webrtc")]
mod js_utils {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    pub enum Global {
        Window(web_sys::Window),
        WorkerGlobal(web_sys::WorkerGlobalScope),
        ServiceWorkerGlobal(web_sys::ServiceWorkerGlobalScope),
    }

    pub fn global() -> Option<Global> {
        let obj = JsValue::from(js_sys::global());
        if obj.has_type::<web_sys::Window>() {
            return Some(Global::Window(web_sys::Window::from(obj)));
        }
        if obj.has_type::<web_sys::WorkerGlobalScope>() {
            return Some(Global::WorkerGlobal(web_sys::WorkerGlobalScope::from(obj)));
        }
        if obj.has_type::<web_sys::ServiceWorkerGlobalScope>() {
            return Some(Global::ServiceWorkerGlobal(
                web_sys::ServiceWorkerGlobalScope::from(obj),
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_notifier() {
        let notifier = Notifier::default();
        notifier.set_timeout(1);

        let mut jobs = vec![];

        // Await three times.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                notifier_clone.await;
            }));
        }

        // Await three times after wake.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                notifier_clone.await;
            }));
        }

        futures::future::join_all(jobs).await;
        notifier.await;
    }
}
