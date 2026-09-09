# Browser Node and Extension

The frontend at [rings.rs](https://rings.rs) is the same node compiled to WebAssembly and
wrapped in a Rust/Yew application. It runs as a web app, and the same build packages as a
Chrome extension. Its source is the repository's
[`frontend`](https://github.com/RingsNetwork/rings/tree/master/frontend) workspace.

## The hosted console

[rings.rs/#node](https://rings.rs/#node) is a browser node with a console around it. From top
to bottom:

1. **Settings** (`#node/settings`): the network id, ICE servers, stabilization interval, the
   name of the browser storage the node persists to, the seed URL the node joins through
   (`https://node.rings.rs`, the public seed, by default; a native node's external API such
   as `http://127.0.0.1:50001` is the other suggestion), and the WebView's onion policy.
   Settings are kept in the browser's local storage.
2. **Account**: the identity the node signs with. WebCrypto P-256 generates a key in the
   browser; MetaMask signs with EIP-191; Phantom signs with Ed25519. The node's DID is derived
   from the chosen account.
3. **Start**: builds the provider from the settings and account and starts listening.
4. **Connect**: through a seed node's HTTP endpoint (the same public handshake
   [`rings connect node`](cli.md#through-a-peers-http-endpoint) uses), or by hand with an SDP
   offer and answer pasted between two browsers.
5. **Topology**: the connected peers drawn on the Chord ring, with the node's own position.
6. **Workbench** (`#node/workbench`): the Onion Proxy panel builds privacy-layer routes through
   onion relays and exits and sends HTTPS requests over them; the Custom panel registers a
   namespace and sends and receives messages under it over the plain overlay, which routes and
   encrypts but does not hide the endpoints.
7. **WebView**: once the local onion gateway is ready, `/webview` browses target sites through
   onion circuits, answered by a service worker the shell registers.

The shell is hash-routed (`#node`, `#node/settings`, `#node/workbench`, `#guide`), so every
screen has a URL.

## Run it locally

Install [Trunk](https://trunkrs.dev/) and the `wasm32-unknown-unknown` target, then:

```bash
cd frontend
trunk serve --release true
```

Trunk prints the URL it serves. The frontend is its own Cargo workspace, so its checks run from
that directory:

```bash
cargo check --target wasm32-unknown-unknown
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test --release --target wasm32-unknown-unknown
```

## Package the extension

The extension is the same application rewritten into a Chrome Manifest V3 package: a side
panel and options page over a retained offscreen node, a wallet bridge that asks Chrome to
inject a short-lived signing request into the active `http`/`https` tab for MetaMask and
Phantom, and the Onion WebView. From the repository root:

```bash
npm ci --ignore-scripts
npm run package:frontend-extension
```

The package is written to `frontend/dist-extension/`. Load that directory from
`chrome://extensions` with developer mode on; after each rebuild, click Reload there.

Two boundaries of the package are deliberate. The wallet bridge needs a normal wallet-enabled
page open as the active tab, because that is where the wallet provider is injected; there is no
centralized bridge site. The WebView renders proxied documents with a CSP `sandbox` and no
`allow-same-origin`, so target script stays in an opaque origin; static and server-rendered
pages with rewritten subresources are supported, and page-driven `fetch` or XHR to the gateway
is not. The
[frontend README](https://github.com/RingsNetwork/rings/blob/master/frontend/README.md)
records both in full, with the test suites that pin them.

## Embed the node in your own page

For your own application, skip the console and use the npm package directly; [Build for
Wasm](build-for-wasm.md) covers the package and a custom build, and
[Connect Rings Network](introduction/connect-rings-network.md) the ways a browser node joins.
