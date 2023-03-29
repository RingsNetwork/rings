use wasm_bindgen::JsCast;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::prelude::wasm_bindgen::JsValue;
use crate::utils::js_utils;

#[wasm_bindgen_test]
async fn test_window_sleep_not_panic() {
    js_utils::window_sleep(200).await.unwrap();
}

#[wasm_bindgen_test]
async fn test_global() {
    let obj = JsValue::from(js_sys::global());
    assert!(obj.has_type::<web_sys::Window>());
    assert!(!obj.has_type::<web_sys::WorkerGlobalScope>());
}
