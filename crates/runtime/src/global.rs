//! The JavaScript global scope a wasm build runs under, and the one primitive Rings schedules
//! on it: a zero-argument timeout.
//!
//! A browser has three global scopes that own `setTimeout`; which one is current is a
//! property of the runtime, not of the caller, so it is detected here once and the timer
//! ([`crate::sleep`]) schedules on whichever it finds — a page and a (service) worker alike.

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

/// The JavaScript global scopes Rings can schedule on.
pub enum Global {
    /// Browser window global scope.
    Window(web_sys::Window),
    /// Dedicated or shared worker global scope.
    Worker(web_sys::WorkerGlobalScope),
    /// Service worker global scope.
    ServiceWorker(web_sys::ServiceWorkerGlobalScope),
}

impl Global {
    /// Schedule a zero-argument timeout callback on this global scope.
    pub fn set_timeout_0(&self, callback: &js_sys::Function, millis: i32) -> Result<i32, JsValue> {
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

/// Detect the current JavaScript global scope; `None` outside the three scopes above.
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
