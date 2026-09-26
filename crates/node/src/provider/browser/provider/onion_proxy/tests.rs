use std::sync::Arc;

use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::error::Error;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionRole;
use crate::onion::OnionServiceName;
use crate::processor::ProcessorConfig;
use crate::provider::Provider;
use crate::tests::wasm::new_provider;
use crate::tests::wasm::prepare_processor_with_onion_role;

/// Law (one runtime per node): every browser proxy is built over the node's single installed
/// onion runtime. The first proxy installs it and the second reuses it, where a second install
/// would be refused (the data plane's namespace registers once).
#[wasm_bindgen_test]
async fn test_browser_proxies_share_the_installed_runtime() {
    let provider = new_provider().await;
    assert!(!provider.extensions().contains(ONION_CIRCUIT_NAMESPACE));

    provider
        .onion_https_proxy()
        .map_err(JsValue::from)
        .expect("first proxy installs the onion runtime");
    assert!(provider.extensions().contains(ONION_CIRCUIT_NAMESPACE));
    provider
        .onion_https_proxy()
        .map_err(JsValue::from)
        .expect("second proxy reuses the onion runtime");
    assert!(provider
        .onion_runtime
        .lock()
        .expect("runtime slot lock")
        .is_some());
}

/// A browser config of a fresh node registering the onion symbols of `role`.
fn config_with_onion_role(role: OnionRole<OnionExitOffer>) -> ProcessorConfig {
    let key = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegatee key");
    ProcessorConfig::new(0, "stun://stun.l.google.com:19302".to_string(), key, 200).onion_role(role)
}

/// An exit offering `service` to the fixture target.
fn exit_offering(service: OnionServiceName) -> OnionRole<OnionExitOffer> {
    let policy = OnionExitPolicy::from_target_strings(vec!["1.1.1.1:443".to_string()], Vec::new())
        .expect("open policy");
    OnionRole::Exit(OnionExitOffer::new([service], policy).expect("exit offer"))
}

/// A fresh IndexedDB namespace.
fn storage_name() -> String {
    uuid::Uuid::new_v4().to_simple().to_string()
}

/// The production browser constructor rejects an exit offering a service the browser runtime
/// cannot interpret, and installs the data plane at start for every role that registers
/// `relay`.
#[wasm_bindgen_test]
async fn test_browser_constructor_admits_only_roles_it_can_interpret() {
    let rejected = Provider::new_browser_provider_with_storage(
        config_with_onion_role(exit_offering(OnionServiceName::tcp())),
        storage_name(),
    )
    .await;

    assert!(matches!(
        rejected,
        Err(Error::UninterpretableOnionService { service }) if service == OnionServiceName::tcp()
    ));
    for role in [OnionRole::Relay, exit_offering(OnionServiceName::https())] {
        let provider = Provider::new_browser_provider_with_storage(
            config_with_onion_role(role),
            storage_name(),
        )
        .await
        .expect("browser provider");
        assert!(provider.extensions().contains(ONION_CIRCUIT_NAMESPACE));
    }
}

/// A provider built around an existing relay processor installs its data plane when it
/// starts listening, before it publishes its relay registration.
#[wasm_bindgen_test]
async fn test_browser_listen_installs_the_runtime_of_a_relay() {
    let processor = prepare_processor_with_onion_role(OnionRole::Relay).await;
    let provider = Provider::from_processor(Arc::new(processor));
    provider.set_backend().expect("install backend");
    assert!(!provider.extensions().contains(ONION_CIRCUIT_NAMESPACE));

    let listener = provider.listen();
    JsFuture::from(listener.started())
        .await
        .expect("listener started");
    listener.stop();

    assert!(provider.extensions().contains(ONION_CIRCUIT_NAMESPACE));
}
