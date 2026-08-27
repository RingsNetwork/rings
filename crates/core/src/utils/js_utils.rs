use std::future::Future;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

/// JavaScript global scope variants supported by Rings wasm utilities.
pub enum Global {
    /// Browser window global scope.
    Window(web_sys::Window),
    /// Dedicated or shared worker global scope.
    WorkerGlobal(web_sys::WorkerGlobalScope),
    /// Service worker global scope.
    ServiceWorkerGlobal(web_sys::ServiceWorkerGlobalScope),
}

impl Global {
    /// Schedule a zero-argument timeout callback on this global scope.
    pub fn set_timeout_0(&self, callback: &js_sys::Function, millis: i32) -> Result<i32, JsValue> {
        match self {
            Global::Window(global) => {
                global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
            }
            Global::WorkerGlobal(global) => {
                global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
            }
            Global::ServiceWorkerGlobal(global) => {
                global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
            }
        }
    }
}

/// Detect the current JavaScript global scope.
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

fn resolve_sleep(resolve: &js_sys::Function) {
    if let Err(error) = resolve.call0(&JsValue::NULL) {
        tracing::error!("Failed to resolve sleep promise: {:?}", error);
    }
}

fn reject_sleep(reject: &js_sys::Function, error: JsValue) {
    if let Err(reject_error) = reject.call1(&JsValue::NULL, &error) {
        tracing::error!("Failed to reject sleep promise: {:?}", reject_error);
    }
}

fn schedule_sleep<F>(resolve: js_sys::Function, reject: js_sys::Function, schedule: F)
where F: FnOnce(&js_sys::Function) -> Result<i32, JsValue> {
    let func = Closure::once_into_js(move || {
        resolve_sleep(&resolve);
    });
    let callback = func.as_ref().unchecked_ref();
    if let Err(error) = schedule(callback) {
        tracing::error!("Failed to schedule sleep timeout: {:?}", error);
        reject_sleep(&reject, error);
    }
}

/// Return a JavaScript future that resolves after `millis` milliseconds.
pub fn window_sleep(millis: i32) -> wasm_bindgen_futures::JsFuture {
    let promise = match global() {
        None => js_sys::Promise::reject(&JsValue::from_str("No global scope for window_sleep")),
        Some(global) => js_sys::Promise::new(&mut move |resolve, reject| {
            schedule_sleep(resolve, reject, |callback| {
                global.set_timeout_0(callback, millis)
            });
        }),
    };
    wasm_bindgen_futures::JsFuture::from(promise)
}

/// Spawn a wasm-local interval loop that waits for each tick task to finish.
pub fn spawn_interval<F, Fut>(millis: i32, mut task: F)
where
    F: FnMut() -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            if let Err(error) = window_sleep(millis).await {
                tracing::error!("failed to wait for interval tick: {:?}", error);
                return;
            }

            task().await;
        }
    });
}
