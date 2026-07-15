# Rings Frontend

Repository-level browser frontend for Rings. This replaces the historical
browser connectivity example as the shared landing guide, web app, and
browser-extension package. The standalone `dweb` and `proof-demo` surfaces remain
conceptually separate; this frontend includes onion proxy, proof, and
custom-message workbench panels for operating a browser node from one screen.

The implementation is Rust/Yew. Browser APIs for WebCrypto, MetaMask, and Phantom
are called from Rust through `js_sys` and `wasm_bindgen`; the core application has
no JavaScript source. Extension packaging uses strict TypeScript for the MV3
service worker, wallet bridge, node bridge, and packaging scripts. The build emits
the JavaScript files Chrome and Node load at runtime into ignored generated output
directories; JavaScript is not checked in as source.
The TypeScript gate runs Biome plus `tsc`: Biome enforces formatting, import
organization, `noExplicitAny`, unused-symbol checks, `noVar`, and no non-null
assertions; `tsc` runs strict type checks with no implicit `any`, unchecked
index access, unused locals/parameters, and no emit on error. The docs check
also requires file-level docs and top-level type, interface, function, class, or
enum declarations to have JSDoc.

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
- Present a landing guide with links into the node console and GitHub.
- Build HTTPS onion proxy routes and send HTTPS requests through onion exits.
- Run the distributed SNARK proof workbench flow alongside the retained proof demo.
- Register and send user custom namespace messages.

## Run as a Web App

```sh
cd frontend
trunk serve --release true
```

Then open the Trunk URL. The release profile avoids debug wasm-bindgen local
limits from the proof stack while keeping the application source Rust/Yew-only.
Use the guide as the landing page, then open the node console for connection,
onion proxy, proof, and custom-message workflows.

## Package as a Chrome Extension

Build the same Yew/Wasm application with Trunk. The Trunk `post_build` hook first
type-checks and compiles the extension TypeScript into `.generated/`, then
rewrites the web output into a Chrome Manifest V3 package after every build:

```sh
cd frontend
npm --prefix .. ci --ignore-scripts
npm --prefix .. run build:frontend-extension-scripts
trunk build --release
```

The extension is written to `dist-extension/`. Load that directory from
`chrome://extensions` with developer mode enabled. When Chrome already has this
unpacked extension loaded, click Reload after each Trunk rebuild.

The extension package differs from the web package in several ways:

- `manifest.json` declares a MV3 extension with a side panel and options page.
- `host_permissions` covers ordinary `http`/`https` pages so the wallet bridge
  can reach wallet providers injected into the active tab.
- The side panel talks to the retained offscreen node for onion proxy route and
  request calls. This is an extension-owned proxy client, not a Chrome global
  proxy setting or URL-hijacking layer.
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
cd ..
npm ci --ignore-scripts
npx playwright install chromium
npm run test:frontend-extension-wallet
```

The test opens `test-pages/wallet-fixture.html` from a local `127.0.0.1` server,
loads `dist-extension/` as an unpacked extension, and verifies MetaMask and
Phantom bridge calls against mock providers in the current tab.

## Check

```sh
cd frontend
cargo fmt --check
cargo check --target wasm32-unknown-unknown
cargo test --release --target wasm32-unknown-unknown
npm --prefix .. ci --ignore-scripts
npm --prefix .. run build:frontend-extension-scripts
trunk build --release
cd ..
npx playwright install chromium
npm run test:frontend-extension-wallet
```
