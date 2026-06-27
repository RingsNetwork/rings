#![cfg(target_arch = "wasm32")]
//! Browser integration test (wasm-bindgen-test) for the Rust `Echo` helper protocol.
//!
//! The browser page in `index.html` registers a JS `example` protocol through
//! `provider.on(...)`. This test intentionally covers the separate Rust helper path in
//! `src/lib.rs`, where applications can register the built-in `Echo` protocol from WASM.
//! It exercises the pure `Echo::step` transition with no overlay, so it stays deterministic.

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_node::extension::ext::Ctx;
use rings_node::extension::ext::Protocol;
use rings_node::extension::ext::Wire;
use rings_node::extension::protocols::echo::Echo;
use rings_node::extension::protocols::echo::EchoEffect;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen_test]
fn echo_replies_and_counts() {
    let did = Did::from(SecretKey::random().address());
    let echo = Echo;
    let state = echo.init();

    let event = echo
        .decode(Wire {
            from: did,
            me: did,
            payload: b"hi",
        })
        .expect("decode");
    let transition = echo.step(Ctx { did, state: &state }, event);

    assert_eq!(transition.state, 1, "echo counts the message");
    match transition.effects.as_slice() {
        [EchoEffect::Reply { to, payload }] => {
            assert_eq!(*to, did);
            assert_eq!(payload.as_ref(), b"hi");
        }
        other => panic!("expected a single echo Reply, got {other:?}"),
    }
}
