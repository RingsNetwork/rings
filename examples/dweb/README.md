# Rings dweb (Yew)

A Rust/[Yew](https://yew.rs) rewrite of the (deprecated, TypeScript) `rings-dweb`. A
self-contained **decentralized web** demo: every node both *hosts* a tiny static site
and *fetches* pages from peers, peer-to-peer over rings — no central server.

It all runs over a single `dweb` namespace registered with `provider.on(..)` (the
JsProtocol path): the handler answers a request with one `Send` effect carrying the
hosted page, and surfaces responses to the UI. Messages are JSON —
`{"kind":"req","path":"/"}` / `{"kind":"res","path":"/","body":"…"}`.

## Run

```sh
cargo install trunk          # one-time
trunk serve --port 8081      # open two tabs/instances
```

Open two instances (e.g. two browsers or two `trunk serve` ports), copy one node's DID
into the other's "peer DID" box, and fetch `/` — you'll get the page that peer hosts,
delivered over the overlay. (Two peers must be able to reach each other; see the relay
example notes on connectivity.)

Standalone wasm crate (excluded from the cargo workspace); only builds for wasm. A
committed `Cargo.lock` pins deps to wasm-buildable versions.
