# Rings dweb (Yew)

A Rust/[Yew](https://yew.rs) rewrite of the (deprecated, TypeScript) `rings-dweb`.

## Functionality

A self-contained **decentralized web**: every node is at once a tiny static-site *host*
and a *browser*, exchanging pages peer-to-peer over rings with no central server.

- On start it builds an in-browser rings node (IndexedDB storage), installs the extension
  backend, and registers a `dweb` protocol via `provider.on(..)`.
- The `dweb` handler is `(ctx, event) -> { state, effects }`:
  - a **request** `{"kind":"req","path":"/"}` is answered with one `Send` effect carrying
    the hosted page (`{"kind":"res","path":"/","body":"…"}`), or a 404;
  - a **response** is surfaced to the UI.
- The UI shows this node's DID and the page it hosts, and lets you fetch a `path` from a
  peer's DID — the page is delivered over the overlay and rendered.

The rings wiring lives in `src/lib.rs` (`build_node`, `dweb_handle`, `register_dweb`,
`fetch_path`); `src/main.rs` just mounts the Yew app.

## Security model

A fetched page is **peer-controlled, untrusted HTML**. It is never injected into the app's
DOM; instead it is rendered inside an `<iframe srcdoc=… sandbox="">`. The empty `sandbox`
attribute applies *all* restrictions — scripts are disabled, the frame has an opaque origin,
and forms/popups/top-navigation are blocked — so a hostile page cannot run JavaScript, read
cookies/storage, or reach the host app's origin or DOM.

## Run

```sh
cargo install trunk          # one-time
trunk serve                  # → http://localhost:8080
```

Open two instances, paste one node's DID into the other's "peer DID" box, and fetch `/`.

## Test

Runs in a real **headless browser** via the wasm-bindgen-test toolkit; `webdriver.json`
supplies the Chrome launch flags:

```sh
wasm-pack test --headless --chrome   # 4 tests
```

Requires a `chromedriver` whose version matches your installed Chrome (a mismatch makes
Chrome exit on launch). Firefox works too via `--firefox` + a matching `geckodriver`.
