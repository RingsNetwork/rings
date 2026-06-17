# Rings browser (wasm) example — dweb core

The UI-free core a browser (e.g. Yew) Rings app calls. It shows that **the browser uses
the same model as native**: protocols are pure Rust `Protocol`s. Here the built-in Rust
`Echo` protocol is registered on a *browser* `Provider` — no JavaScript handler, the
same code a daemon runs. (For a JS-defined protocol, use
`provider.on(namespace, initialState, handler)` instead.)

Three primitives (`src/lib.rs`), which a frontend's components call:

- `connect_via_seed(provider, url)` — join an overlay via a seed node's HTTP endpoint;
- `register_echo(provider)` — register the protocol(s) this app speaks;
- `send_echo(provider, did, payload)` — send a namespaced message to a peer.

The whole crate is `#![cfg(target_arch = "wasm32")]`, so on native it is an empty lib
(the workspace stays green) and only builds for real on wasm.

## Build

```sh
wasm-pack build examples/browser
```

## Test

```sh
wasm-pack test --headless --chrome examples/browser
```

`tests/echo.rs` checks the pure `Echo::step` the example installs (deterministic, no
overlay). The full two-node browser round-trip (connect + namespaced protocol + send)
is covered by the node crate's `crates/node/src/tests/wasm/browser.rs` suite, which has
the in-browser connection harness.
