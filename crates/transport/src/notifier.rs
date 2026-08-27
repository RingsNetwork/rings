//! This module contains the [Notifier] struct.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Context;
use std::task::Poll;

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use crate::core::transport::WebrtcConnectionState;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use crate::error::Error;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use crate::error::Result;
use crate::sync_utils::lock_recover;

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
    fn state(&self) -> MutexGuard<'_, NotifierState> {
        lock_recover(&self.0)
    }

    /// Immediately wake the notifier.
    pub fn wake(&self) {
        let mut state = self.state();
        state.woken = true;
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
    }

    /// Wake the notifier after the specified time.
    #[cfg(not(any(
        all(feature = "web-sys-webrtc", target_family = "wasm"),
        all(feature = "native-webrtc", not(target_family = "wasm"))
    )))]
    pub fn set_timeout(&self, seconds: u8) {
        self.set_timeout_ms(u64::from(seconds) * 1000);
    }

    /// Wake the notifier after the specified time.
    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    pub fn set_timeout(&self, seconds: u8) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(seconds.into())).await;
            this.wake();
        });
    }

    /// Wake the notifier after the specified number of milliseconds.
    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    pub fn set_timeout_ms(&self, millis: u64) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(millis)).await;
            this.wake();
        });
    }

    /// Wake the notifier after the specified number of milliseconds.
    #[cfg(not(any(
        all(feature = "web-sys-webrtc", target_family = "wasm"),
        all(feature = "native-webrtc", not(target_family = "wasm"))
    )))]
    pub fn set_timeout_ms(&self, millis: u64) {
        native_timeout_scheduler::schedule_wake(self.clone(), millis);
    }

    /// Wake the notifier after the specified time.
    #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
    pub fn set_timeout(&self, seconds: u8) {
        self.set_timeout_ms(u64::from(seconds) * 1000);
    }

    /// Wake the notifier after the specified number of milliseconds.
    #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
    pub fn set_timeout_ms(&self, millis: u64) {
        use wasm_bindgen::JsCast;

        let millis = i32::try_from(millis).unwrap_or(i32::MAX);

        let timeout_notifier = self.clone();
        let fallback_notifier = self.clone();
        let wake = wasm_bindgen::closure::Closure::once_into_js(move || {
            timeout_notifier.wake();
        });

        let Some(global) = js_utils::global() else {
            fallback_notifier.wake();
            return;
        };

        let callback = wake.as_ref().unchecked_ref();
        let scheduled = global.set_timeout_0(callback, millis);
        if scheduled.is_err() {
            fallback_notifier.wake();
        }
    }
}

impl Future for Notifier {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state();

        if state.woken {
            return Poll::Ready(());
        }

        state.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(crate) async fn wait_for_data_channel_open(
    state: WebrtcConnectionState,
    data_channel_is_open: impl Fn() -> Result<bool>,
    notifier: &Notifier,
    timeout_seconds: u8,
) -> Result<()> {
    // `Disconnected` remains eligible: buffered bytes may flush after ICE
    // recovery, and the delivery future observes whether that happened.
    if state.is_terminal() {
        return Err(Error::DataChannelOpen("Connection unavailable".to_string()));
    }
    if data_channel_is_open()? {
        return Ok(());
    }

    notifier.set_timeout(timeout_seconds);
    notifier.clone().await;

    if data_channel_is_open()? {
        Ok(())
    } else {
        Err(Error::DataChannelOpen(format!(
            "DataChannel not open in {timeout_seconds} seconds"
        )))
    }
}

#[cfg(not(any(
    all(feature = "web-sys-webrtc", target_family = "wasm"),
    all(feature = "native-webrtc", not(target_family = "wasm"))
)))]
mod native_timeout_scheduler;

// This is copied from utils module of rings-core crate.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
mod js_utils {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    pub enum Global {
        Window(web_sys::Window),
        Worker(web_sys::WorkerGlobalScope),
        ServiceWorker(web_sys::ServiceWorkerGlobalScope),
    }

    impl Global {
        pub fn set_timeout_0(
            &self,
            callback: &js_sys::Function,
            millis: i32,
        ) -> Result<i32, JsValue> {
            match self {
                Global::Window(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::Worker(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::ServiceWorker(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
            }
        }
    }

    pub fn global() -> Option<Global> {
        let obj = JsValue::from(js_sys::global());
        if obj.has_type::<web_sys::Window>() {
            return Some(Global::Window(web_sys::Window::from(obj)));
        }
        if obj.has_type::<web_sys::WorkerGlobalScope>() {
            return Some(Global::Worker(web_sys::WorkerGlobalScope::from(obj)));
        }
        if obj.has_type::<web_sys::ServiceWorkerGlobalScope>() {
            return Some(Global::ServiceWorker(
                web_sys::ServiceWorkerGlobalScope::from(obj),
            ));
        }
        None
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test_notifier;
