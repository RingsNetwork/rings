#![cfg(target_arch = "wasm32")]
//! Browser integration test (wasm-bindgen-test) for the protocol the example registers.
//!
//! It exercises the pure `Echo::step` the example installs — deterministic, no overlay
//! needed — so it runs reliably under `wasm-pack test`. The full two-node browser
//! round-trip (connect + namespaced protocol + send) is covered by the node crate's
//! `tests/wasm/browser.rs` suite, which has the in-browser connection harness.

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_node::backend::ext::Ctx;
use rings_node::backend::ext::Effect;
use rings_node::backend::ext::Event;
use rings_node::backend::ext::Protocol;
use rings_node::backend::protocols::echo::Echo;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen_test]
fn echo_replies_and_counts() {
    let did = Did::from(SecretKey::random().address());
    let echo = Echo::default();
    let state = echo.init();

    let event = Event {
        from: did,
        payload: Bytes::from_static(b"hi"),
    };
    let transition = echo.step(Ctx { did, state: &state }, &event);

    assert_eq!(transition.state, 1, "echo counts the message");
    match transition.effects.as_slice() {
        [Effect::Send {
            to,
            namespace,
            payload,
        }] => {
            assert_eq!(*to, did);
            assert_eq!(namespace, "echo");
            assert_eq!(payload.as_ref(), b"hi");
        }
        other => panic!("expected a single echo Send, got {other:?}"),
    }
}
