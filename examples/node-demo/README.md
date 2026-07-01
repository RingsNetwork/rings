# Rings node demo

Unified browser node demo for Rings. This replaces the separate browser connectivity,
dweb, and proof-demo surfaces as the future node demo and website app.

The implementation is Rust/Yew. Browser APIs for WebCrypto, MetaMask, and Phantom
are called from Rust through `js_sys` and `wasm_bindgen`; this example has no JS or
TS application source.

Styles are split under `src/styles/` by responsibility:

- `base.css`: document defaults and native controls.
- `layout.css`: page shell, panels, rows, grids, and tabs.
- `components.css`: reusable form, text, status, list, and iframe classes.
- `features.css`: feature-specific surfaces such as topology rendering.
- `responsive.css`: viewport-specific rules.

## Features

- Start a browser Rings node with WebCrypto P-256, MetaMask EIP-191, or Phantom Ed25519.
- Connect by SDP offer/answer or by a seed node HTTP endpoint.
- Render connected peers as a circular topology.
- Host and fetch dweb pages over the `dweb` namespace.
- Run the distributed SNARK proof flow from the previous proof demo.
- Register and send user custom namespace messages.

## Run

```sh
cd examples/node-demo
trunk serve --release true
```

Then open the Trunk URL. The release profile avoids debug wasm-bindgen local
limits from the proof stack while keeping the application source Rust/Yew-only.
Start a node first, then use the tabs for connection, dweb, proof, and custom
message workflows.

## Check

```sh
cd examples/node-demo
cargo fmt --check
cargo check --target wasm32-unknown-unknown
```
