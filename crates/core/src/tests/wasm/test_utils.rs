use std::cell::Cell;
use std::rc::Rc;

use futures::FutureExt;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::message::browser_task_yield_guard_counts_for_test;
use crate::message::reset_browser_task_yield_guard_counts_for_test;
use crate::message::yield_browser_task;
use crate::message::yield_core_actor_step;
use crate::message::CORE_ACTOR_BROWSER_YIELD_INTERVAL;
use crate::utils::js_utils;

#[wasm_bindgen_test]
async fn test_window_sleep_not_panic() {
    js_utils::window_sleep(200).await.unwrap();
}

#[wasm_bindgen_test]
async fn test_core_actor_steps_yield_to_a_queued_browser_task() {
    let channel = web_sys::MessageChannel::new().unwrap();
    let observed = Rc::new(Cell::new(false));
    let callback_observed = observed.clone();
    let callback = Closure::wrap(Box::new(move |_event: web_sys::MessageEvent| {
        callback_observed.set(true);
    }) as Box<dyn FnMut(_)>);
    let port = channel.port1();
    port.set_onmessage(Some(callback.as_ref().unchecked_ref()));
    channel.port2().post_message(&JsValue::NULL).unwrap();

    for _ in 0..CORE_ACTOR_BROWSER_YIELD_INTERVAL {
        yield_core_actor_step().await;
    }

    assert!(observed.get());
    port.set_onmessage(None);
}

#[wasm_bindgen_test]
async fn test_cancelled_browser_task_yield_clears_its_js_handler() {
    reset_browser_task_yield_guard_counts_for_test();
    assert!(yield_browser_task().now_or_never().is_none());
    assert_eq!(browser_task_yield_guard_counts_for_test(), (0, 1));
}

#[wasm_bindgen_test]
async fn test_global() {
    let obj = JsValue::from(js_sys::global());
    assert!(obj.has_type::<web_sys::Window>());
    assert!(!obj.has_type::<web_sys::WorkerGlobalScope>());
}
