#![cfg(target_arch = "wasm32")]
//! Core, UI-free building blocks for a browser (Yew/wasm) Rings app — the "dweb" core
//! extracted from a frontend.
//!
//! The point this example makes: **the browser uses the exact same model as native.**
//! A protocol is a pure [`Protocol`](rings_node::extension::ext::Protocol); here we
//! register the built-in Rust [`Echo`] protocol on a *browser* [`Provider`] — no
//! JavaScript handler, the same code that runs on a daemon. (For a JS-defined protocol
//! use `provider.on(namespace, initialState, handler)` instead.)
//!
//! The three primitives a frontend needs:
//! - [`connect_via_seed`] — join an overlay by dialing a seed node's HTTP endpoint;
//! - [`register_echo`] — register the protocol(s) this app speaks;
//! - [`send_echo`] — send a namespaced message to a peer.
//!
//! UI (Yew components, routing, DOM) is intentionally omitted; these functions are what
//! the components call. Build with `wasm-pack build examples/browser`.

use rings_node::extension::protocols::echo::Echo;
use rings_node::extension::protocols::echo::EchoShell;
use rings_node::provider::Provider;
use wasm_bindgen::prelude::*;

/// Register the built-in Rust `echo` protocol on a browser provider, then install the
/// inbound backend so envelopes are dispatched. After this, any `echo` message the peer
/// sends is replied to with the same payload — identical behaviour to a native node.
#[wasm_bindgen]
pub fn register_echo(provider: &Provider) -> Result<(), JsError> {
    provider
        .register_protocol(Echo, EchoShell)
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Join an overlay by dialing a seed node's JSON-RPC/HTTP endpoint; resolves to the
/// seed's DID. Returns the underlying promise so the caller can `await` it.
#[wasm_bindgen]
pub fn connect_via_seed(provider: &Provider, seed_http_url: String) -> js_sys::Promise {
    provider.connect_peer_via_http(seed_http_url)
}

/// Send a payload to `destination_did` under the `echo` namespace. Resolves to the
/// transaction id. This is the uniform upper-layer send — the transport underneath
/// (WebTransport in browser) is invisible to the caller.
#[wasm_bindgen]
pub fn send_echo(
    provider: &Provider,
    destination_did: String,
    payload: Vec<u8>,
) -> js_sys::Promise {
    let bytes = js_sys::Uint8Array::from(payload.as_slice());
    provider.send_message(destination_did, "echo".to_string(), bytes)
}
