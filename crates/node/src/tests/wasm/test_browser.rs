use std::time::Duration;

use rings_rpc::protos::rings_node::SendBackendMessageRequest;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use super::create_connection;
use super::get_peers;
use super::new_provider;
use super::provider_did;
use super::within_hang_guard;
use crate::prelude::rings_core::utils::js_value;
use crate::provider::browser;

/// Admission and retirement of a provider-level connection, observed through `listPeers`.
///
/// ```text
/// Admitted(a, b) ≡ peer_admitted(b) observed by a's backend
/// Listed(a, b)   ≡ listPeers(a) = [b in state Connected]
///
/// create_connection(a, b)    ⊢ ◇Admitted(a, b) ∧ ◇Admitted(b, a)
/// Admitted(a, b)             ⟹ Listed(a, b)
/// disconnect(a, b)           ⊢ ◇Retired(a, b),  Retired(a, b) ⟹ listPeers(a) = []
/// ```
///
/// Each backend records transitions from provider creation on, before any connection exists,
/// so an admission or retirement that lands before its wait is still in the log. The swarm
/// emits `Connected` only once the record is admitted, and `PeerRetired` only after the record
/// has been retired, so both listings are read after the transition they depend on.
#[wasm_bindgen_test]
async fn test_two_provider_connect_and_list() {
    // super::setup_log();
    let node1 = new_provider().await;
    let node2 = new_provider().await;

    let _listen1 = node1.provider.listen();
    let _listen2 = node2.provider.listen();

    create_connection(&node1, &node2).await;

    let peers = get_peers(&node1.provider).await;
    assert!(peers.len() == 1, "peers len should be 1");
    let peer2 = peers.first().unwrap();

    assert_eq!(
        peer2.state, "Connected",
        "peer2 state got {:?}",
        peer2.state
    );

    JsFuture::from(node1.provider.disconnect(peer2.did.clone(), None))
        .await
        .unwrap();
    node1
        .transitions
        .retired(provider_did(&node2.provider))
        .await;
    let peers = get_peers(&node1.provider).await;
    assert_eq!(peers.len(), 0);
}

/// Verifies that browser listener generations serialize startup and release
/// the processor lifecycle lock after cooperative shutdown.
///
/// The test first holds the processor lifecycle lock as a synthetic old generation and proves the
/// replacement's `started` promise stays pending. It then releases the lock,
/// waits for startup, stops the listener, and repeats the full lifecycle three
/// times to prove cleanup does not leave the lock permanently owned.
#[wasm_bindgen_test]
async fn test_provider_listener_handle_requests_stop() {
    let provider = new_provider().await.provider;

    // Hold the processor lifecycle lock to model a previous listener generation
    // still cleaning up after `stop`.
    let listener_lifecycle_lock = provider.listener_lifecycle_lock_for_test();
    let old_cleanup = listener_lifecycle_lock.lock().await;
    let waiting_listener = provider.listen();
    // `started` must remain pending while `old_cleanup` owns the processor lifecycle lock.
    let started = Box::pin(JsFuture::from(waiting_listener.started()));
    let short_delay = Box::pin(rings_runtime::sleep(Duration::from_millis(10)));
    let pending_started = match futures::future::select(started, short_delay).await {
        futures::future::Either::Left(_) => {
            waiting_listener.stop();
            panic!("a new listener started while the previous generation still held the lock");
        }
        futures::future::Either::Right((delay, pending_started)) => {
            delay.unwrap();
            pending_started
        }
    };
    // Releasing the old generation should allow the queued listener to publish
    // its started signal and later finish through cooperative stop.
    drop(old_cleanup);
    pending_started.await.unwrap();
    waiting_listener.stop();
    JsFuture::from(waiting_listener.task()).await.unwrap();

    // Repeated start/stop cycles prove the same lock is released by each task.
    for _ in 0..3 {
        let listener = provider.listen();
        JsFuture::from(listener.started()).await.unwrap();
        assert!(!listener.is_stopped());

        listener.stop();
        assert!(listener.is_stopped());
        JsFuture::from(listener.task()).await.unwrap();
    }
}

/// A backend message is accepted for sending once the destination is admitted.
///
/// `create_connection` returns only after both admissions were observed, so the send runs
/// against an admitted peer instead of racing the handshake.
#[wasm_bindgen_test]
async fn test_send_backend_message() {
    let node1 = new_provider().await;
    let node2 = new_provider().await;

    let _listen1 = node1.provider.listen();
    let _listen2 = node2.provider.listen();

    create_connection(&node1, &node2).await;

    let req = SendBackendMessageRequest {
        destination_did: node2.provider.address(),
        namespace: "text".to_string(),
        // `data` is base64-encoded on the wire (binary-safe).
        data: base64::encode(b"test"),
    };

    JsFuture::from(node1.provider.request(
        "sendBackendMessage".to_string(),
        js_value::serialize(&req).unwrap(),
    ))
    .await
    .unwrap();
}

/// A backend message reaches the receiver's JS protocol handler.
///
/// ```text
/// Admitted(p1, p2) ∧ Admitted(p2, p1) ; send(p1 → p2, "hello world")  ⊢  ◇Handled("hello world")
/// ```
///
/// The handler resolves a `Promise` created before the send. A promise keeps its value, so a
/// delivery that completes before the test awaits it is not lost.
#[wasm_bindgen_test]
async fn test_handle_backend_message() {
    let node1 = new_provider().await;
    let node2 = new_provider().await;

    // The protocol handler reports what it received by resolving this promise.
    let mut resolve_received = None;
    let received = js_sys::Promise::new(&mut |resolve, _reject| {
        resolve_received = Some(resolve);
    });
    js_sys::Reflect::set(
        &js_sys::global(),
        &"resolveReceivedText".into(),
        &resolve_received.unwrap(),
    )
    .unwrap();

    // Register a `text` protocol on provider2 via the unified JsProtocol path: a pure
    // `(ctx, event) -> { state, effects }` handler that reports the received payload.
    let js_code_args = "ctx, event";
    let js_code_body = r#"
    const text = new TextDecoder().decode(event.payload);
    console.log("js protocol: got message", text);
    globalThis.resolveReceivedText(text);
    return { state: ctx.state, effects: [] };
"#;
    let func = js_sys::Function::new_with_args(js_code_args, js_code_body);
    node2
        .provider
        .on("text".to_string(), wasm_bindgen::JsValue::NULL, func)
        .unwrap();

    let _lis1 = node1.provider.listen();
    let _lis2 = node2.provider.listen();

    create_connection(&node1, &node2).await;

    let peers = get_peers(&node1.provider).await;
    assert!(peers.len() == 1, "peers len should be 1");

    let payload = js_sys::Uint8Array::from("hello world".as_bytes());
    JsFuture::from(node1.provider.send_message(
        node2.provider.address(),
        "text".to_string(),
        payload,
    ))
    .await
    .unwrap();
    console_log!("send backend hello world done");
    let ret = within_hang_guard("backend message handled", JsFuture::from(received))
        .await
        .unwrap()
        .as_string()
        .unwrap();
    assert_eq!(&ret, "hello world", "{ret:?}");
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
