use rings_rpc::protos::rings_node::SendBackendMessageRequest;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use super::create_connection;
use super::get_peers;
use super::new_provider;
use crate::prelude::rings_core::utils;
use crate::prelude::rings_core::utils::js_value;
use crate::provider::browser;

#[cfg(feature = "browser_chrome_test")]
wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn test_two_provider_connect_and_list() {
    // super::setup_log();
    let provider1 = new_provider().await;
    let provider2 = new_provider().await;

    let _listen1 = provider1.listen();
    let _listen2 = provider2.listen();

    create_connection(&provider1, &provider2).await;
    console_log!("wait for register");
    utils::js_utils::window_sleep(1000).await.unwrap();

    let peers = get_peers(&provider1).await;
    assert!(peers.len() == 1, "peers len should be 1");
    let peer2 = peers.first().unwrap();

    assert_eq!(
        peer2.state, "Connected",
        "peer2 state got {:?}",
        peer2.state
    );

    JsFuture::from(provider1.disconnect(peer2.did.clone(), None))
        .await
        .unwrap();
    let peers = get_peers(&provider1).await;
    assert_eq!(peers.len(), 0);
}

#[wasm_bindgen_test]
async fn test_send_backend_message() {
    let provider1 = new_provider().await;
    let provider2 = new_provider().await;

    let _listen1 = provider1.listen();
    let _listen2 = provider2.listen();

    create_connection(&provider1, &provider2).await;
    console_log!("wait for register");
    utils::js_utils::window_sleep(1000).await.unwrap();

    let req = SendBackendMessageRequest {
        destination_did: provider2.address(),
        namespace: "text".to_string(),
        // `data` is base64-encoded on the wire (binary-safe).
        data: base64::encode(b"test"),
    };

    JsFuture::from(provider1.request(
        "sendBackendMessage".to_string(),
        js_value::serialize(&req).unwrap(),
    ))
    .await
    .unwrap();
}

#[wasm_bindgen_test]
async fn test_handle_backend_message() {
    let provider1 = new_provider().await;
    let provider2 = new_provider().await;

    // Register a `text` protocol on provider2 via the unified JsProtocol path: a pure
    // `(ctx, event) -> { state, effects }` handler that records the received payload.
    let js_code_args = "ctx, event";
    let js_code_body = r#"
    const text = new TextDecoder().decode(event.payload);
    console.log("js protocol: got message", text);
    window.recentMsg = text;
    return { state: ctx.state, effects: [] };
"#;
    let func = js_sys::Function::new_with_args(js_code_args, js_code_body);
    provider2
        .on("text".to_string(), wasm_bindgen::JsValue::NULL, func)
        .unwrap();

    let _lis1 = provider1.listen();
    let _lis2 = provider2.listen();

    create_connection(&provider1, &provider2).await;
    console_log!("wait for register");

    utils::js_utils::window_sleep(1000).await.unwrap();

    let peers = get_peers(&provider1).await;
    assert!(peers.len() == 1, "peers len should be 1");
    let _peer2 = peers.first().unwrap();

    let payload = js_sys::Uint8Array::from("hello world".as_bytes());
    JsFuture::from(provider1.send_message(provider2.address(), "text".to_string(), payload))
        .await
        .unwrap();
    console_log!("send backend hello world done");
    utils::js_utils::window_sleep(3000).await.unwrap();
    let global = rings_core::utils::js_utils::global().unwrap();
    if let rings_core::utils::js_utils::Global::Window(window) = global {
        let ret = window
            .get("recentMsg")
            .unwrap()
            .to_string()
            .as_string()
            .unwrap();
        assert_eq!(&ret, "hello world", "{ret:?}");
    } else {
        panic!("cannot get dom window");
    }
}

#[wasm_bindgen_test]
async fn test_get_address_from_hex_pubkey() {
    let pk = "02c0eeef8d136b10b862a0ac979eac2ad036f9902d87963ddf0fa108f1e275b9c7";

    let addr_result = browser::get_address_from_hex_pubkey(pk.to_string());
    assert!(addr_result.is_ok(), "addr_result is error");
    let addr = addr_result.ok().unwrap();
    assert!(
        addr.eq_ignore_ascii_case("0xfada88633e01d2f6704a7f2a6ebc57263aca6978"),
        "got addr {addr:?}"
    );
}

#[wasm_bindgen_test]
async fn test_get_address() {
    let expect_address = "0x8b98cf912975b4b6b67ce94882fc25c210a60a60";
    let got_address = browser::get_address(
        "9z1ZTaGocNSAu3DSqGKR6Dqt214X4dXucVd6C53EgqBK",
        browser::AddressType::Ed25519,
    )
    .ok()
    .unwrap();
    assert!(
        expect_address.eq_ignore_ascii_case(got_address.as_str()),
        "got address: {got_address}, expect: {expect_address}"
    );
    let got_address = browser::get_address(expect_address, browser::AddressType::DEFAULT)
        .ok()
        .unwrap();

    assert!(
        got_address.eq_ignore_ascii_case(expect_address),
        "got address: {got_address}, expect: {expect_address}"
    )
}
