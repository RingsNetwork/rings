# Rings Frontend

Browser frontend for Rings. This replaces the historical browser connectivity
example as the shared web and browser-extension app. The standalone `dweb` and
`proof-demo` surfaces remain conceptually separate; this frontend only includes
dweb and proof workbench panels for operating a browser node from one screen.

The implementation is Rust/Yew. Browser APIs for WebCrypto, MetaMask, and Phantom
are called from Rust through `js_sys` and `wasm_bindgen`; the core application has
no JS or TS source. Extension packaging adds only the MV3 manifest, service worker,
and wasm bootstrap files required by Chrome.

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
- Run the distributed SNARK proof workbench flow alongside the retained proof demo.
- Register and send user custom namespace messages.

## Run as a Web App

```sh
cd examples/frontend
trunk serve --release true
```

Then open the Trunk URL. The release profile avoids debug wasm-bindgen local
limits from the proof stack while keeping the application source Rust/Yew-only.
Start a node first, then use the tabs for connection, dweb, proof, and custom
message workflows.

## Package as a Chrome Extension

Build the same Yew/Wasm application with Trunk. The Trunk `post_build` hook
rewrites the web output into a Chrome Manifest V3 package after every build:

```sh
cd examples/frontend
trunk build --release
```

The extension is written to `dist-extension/`. Load that directory from
`chrome://extensions` with developer mode enabled. When Chrome already has this
unpacked extension loaded, click Reload after each Trunk rebuild.

The extension package differs from the web package in three ways:

- `manifest.json` declares a MV3 extension with a side panel and options page.
- `host_permissions` covers ordinary `http`/`https` pages so the wallet bridge
  can reach wallet providers injected into the active tab.
- `bootstrap.js` replaces Trunk's inline module script so extension CSP accepts
  the page.
- `content_security_policy.extension_pages` allows packaged WebAssembly with
  `wasm-unsafe-eval`.

WebCrypto P-256 is the primary supported account provider in the extension page.
MetaMask and Phantom use an extension wallet bridge: the extension asks Chrome to
inject a short-lived wallet request into the current active `http`/`https` tab's
main world, then returns only the account and signature to the Yew app. This does
not require a centralized bridge website, but it does require the user to have a
normal wallet-enabled page open as the active tab.

To test the extension wallet bridge without MetaMask, Phantom, or a remote
website, install the repository JavaScript dependencies and run the local
fixture test after packaging the extension:

```sh
cd ../..
npm install --ignore-scripts --package-lock=false
npx playwright install chromium
npm run test:frontend-extension-wallet
```

The test opens `test-pages/wallet-fixture.html` from a local `127.0.0.1` server,
loads `dist-extension/` as an unpacked extension, and verifies MetaMask and
Phantom bridge calls against mock providers in the current tab.

## Check

```sh
cd examples/frontend
cargo fmt --check
cargo check --target wasm32-unknown-unknown
cargo test --release --target wasm32-unknown-unknown
trunk build --release
cd ../..
npm install --ignore-scripts --package-lock=false
npx playwright install chromium
npm run test:frontend-extension-wallet
```
