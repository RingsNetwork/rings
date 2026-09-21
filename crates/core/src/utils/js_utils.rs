//! Browser interop for the wasm build: a sleep over the JavaScript global scope.
//!
//! The scope detection itself ([`rings_transport::js_global`]) is the transport's, which needs
//! it for its data-channel notifier; this module only schedules on it.

use rings_transport::js_global::global;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

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
