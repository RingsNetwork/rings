use wasm_bindgen_test::wasm_bindgen_test;

use crate::utils::js_utils;

#[wasm_bindgen_test]
async fn test_window_sleep_not_panic() {
    js_utils::window_sleep(200).await.unwrap();
}
