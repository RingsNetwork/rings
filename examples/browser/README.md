# Rings browser connectivity example

This example covers the issue #539 flows:

- browser to browser without manually copying SDP;
- browser to native with bidirectional namespaced messages.

The page in `index.html` uses the browser WASM provider directly. Each browser node joins the
overlay through a seed node HTTP endpoint, then dials a peer by DID with `connectWithDid`.
Messages are sent with `sendBackendMessage` under the `example` namespace.

The page imports `ethers@6.13.5` from jsDelivr to create an EIP-191 signing wallet in the
browser. Running the page therefore needs internet access unless that dependency is vendored or
served locally.

`src/lib.rs` also keeps the smaller Rust/WASM helper functions used by application code that
wants to register the built-in Rust `Echo` protocol from a browser provider.

## Build

From the repository root:

```sh
npm install
npm run wasm_pack
```

The page imports `crates/node/pkg/rings_node.js`, so serve the repository root, not just this
directory:

```sh
python3 -m http.server 8080
```

Open:

```text
http://127.0.0.1:8080/examples/browser/
```

## Seed node

Both browser and native examples use the default Rings network id `1`. Start one native daemon as
the shared seed:

```sh
cargo run -p rings-node --bin rings -- init \
  --location /tmp/rings-seed/config.yaml \
  --session-sk /tmp/rings-seed/session_sk

cargo run -p rings-node --bin rings -- run \
  --config /tmp/rings-seed/config.yaml \
  --external-api-addr 127.0.0.1:50001 \
  --internal-api-port 50000 \
  --storage-path /tmp/rings-seed/storage
```

Use `http://127.0.0.1:50001` as the browser page's seed HTTP endpoint.

## Browser to browser

1. Start the seed node above.
2. Serve this repo on two different origins so each browser node gets separate IndexedDB storage:

   ```sh
   python3 -m http.server 8080
   python3 -m http.server 8081
   ```

3. Open both pages:

   ```text
   http://127.0.0.1:8080/examples/browser/
   http://127.0.0.1:8081/examples/browser/
   ```

4. On both pages, click `Start provider`, then `Connect seed`.
5. Copy browser B's DID into browser A's `Remote peer DID`.
6. Wait a few seconds, or click `List peers` until each page shows the seed as connected.
7. On browser A, click `Connect DID`, then `Send example message`.

No SDP is copied by the user. The seed gives the peers an initial overlay path; `connectWithDid`
performs the peer connection by DID.

## Browser to native

1. Start the seed node.
2. Open the browser page, click `Start provider`, then `Connect seed`.
3. Copy the browser DID.
4. Wait a few seconds, or click `List peers` until the page shows the seed as connected.
5. Start the native example and pass the seed URL plus the browser DID:

   ```sh
   cargo run -p rings-native-example -- http://127.0.0.1:50001 BROWSER_DID
   ```

The native example prints its DID, connects through the same seed, sends an `example` message to
the browser, and then waits for replies. To send back from the browser, paste the native DID into
`Remote peer DID` and click `Send example message`.

## Test

The browser page is an integration example. The deterministic protocol unit for the Rust `Echo`
helper runs in node:

```sh
wasm-pack test --release --node examples/browser
```

The full two-provider browser connection harness lives in
`crates/node/src/tests/wasm/browser.rs` and is covered by the node WASM CI path:

```sh
cargo test -p rings-node --release --target=wasm32-unknown-unknown \
  --features browser_default --no-default-features
```

That harness covers direct offer/answer connection setup, `listPeers`, `sendBackendMessage`, and
`provider.on` message handling. It does not start a native seed daemon, so the seed HTTP join plus
`connectWithDid` flow documented above remains a manual integration path.
