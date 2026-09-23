use std::sync::Arc;

use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::tests::wasm::new_provider;

/// Law (one client per node): every browser proxy sends through the HTTPS client of the node's
/// single installed onion runtime, so browser and native callers share one client implementation
/// and one pending table.
#[wasm_bindgen_test]
async fn test_browser_proxies_send_through_the_installed_runtime_client() {
    let provider = new_provider().await;
    let first = provider
        .onion_https_proxy(3, false)
        .map_err(JsValue::from)
        .expect("first proxy installs the onion runtime");
    let second = provider
        .onion_https_proxy(2, true)
        .map_err(JsValue::from)
        .expect("second proxy reuses the onion runtime");
    let runtime = provider
        .onion_https_runtime
        .lock()
        .expect("runtime slot lock")
        .clone()
        .expect("onion runtime installed");

    assert!(Arc::ptr_eq(&first.client, runtime.client()));
    assert!(Arc::ptr_eq(&second.client, runtime.client()));
}
