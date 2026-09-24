use std::sync::Arc;

use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionRole;
use crate::onion::OnionServiceName;
use crate::provider::Provider;
use crate::tests::wasm::new_provider;
use crate::tests::wasm::prepare_processor_with_onion_role;

/// Law (one client per node): every browser proxy sends through the HTTPS client of the node's
/// single installed onion runtime, so browser and native callers share one client implementation
/// and one pending table.
#[wasm_bindgen_test]
async fn test_browser_proxies_send_through_the_installed_runtime_client() {
    let provider = new_provider().await;
    let first = provider
        .onion_https_proxy()
        .map_err(JsValue::from)
        .expect("first proxy installs the onion runtime");
    let second = provider
        .onion_https_proxy()
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

/// A browser provider around a processor registering the onion symbols of `role`.
async fn provider_with_onion_role(role: OnionRole<OnionExitOffer>) -> Provider {
    let processor = prepare_processor_with_onion_role(role).await;
    let provider = Provider::from_processor(Arc::new(processor));
    provider.set_backend().expect("install backend");
    provider
}

/// An open policy for the `https` fixture target.
fn open_policy() -> OnionExitPolicy {
    OnionExitPolicy::from_target_strings(vec!["1.1.1.1:443".to_string()], Vec::new())
        .expect("open policy")
}

/// A browser's Σ-algebra interprets `https` only: an exit role offering `tcp` is rejected when its
/// runtime is installed, and an `https` exit or a relay installs its circuit protocol.
#[wasm_bindgen_test]
async fn test_browser_runtime_admits_only_roles_it_can_interpret() {
    let tcp_exit = provider_with_onion_role(OnionRole::Exit(
        OnionExitOffer::new([OnionServiceName::tcp()], open_policy()).expect("tcp offer"),
    ))
    .await;
    let https_exit = provider_with_onion_role(OnionRole::Exit(
        OnionExitOffer::new([OnionServiceName::https()], open_policy()).expect("https offer"),
    ))
    .await;
    let relay = provider_with_onion_role(OnionRole::Relay).await;

    assert!(tcp_exit.install_onion_runtime().is_err());
    assert!(!tcp_exit.extensions().contains(ONION_CIRCUIT_NAMESPACE));
    for provider in [&https_exit, &relay] {
        provider
            .install_onion_runtime()
            .map_err(JsValue::from)
            .expect("install onion runtime");
        assert!(provider.extensions().contains(ONION_CIRCUIT_NAMESPACE));
    }
}
