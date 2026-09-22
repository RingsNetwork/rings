<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings-white.svg">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings.svg" width="128" height="128">
</picture>

# Rings Transport
======================

[![rings-node](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings)


This crate encompasses the transport layer implementations for the Rings Network, specifically designed for seamless integration in various computing environments. It is integral for enabling effective network communication within both native and browser contexts. The crate includes two primary Rust-based implementations:

## Implementations

* Native Transport

Based on `webrtc.rs`, for building native usecase.

To build for native webrtc:

```sh
cargo build --features native-webrtc
```

or

```sh
make native

```

* WebSys Transport

Based on `wasm_bindgen`, `web_sys`, for Browser usecase

To build for webrtc in browser:

```sh
cargo build --target wasm32-unknown-unknown --features web-sys-webrtc --no-default-features
```

or

```sh
make web
```

* Dummy Transport

This implementation is only use for testcase.


```sh
cargo build --features dummy
```

or

```sh
make dummy
```


## Tests

```sh
cargo test --features native-webrtc
```

```sh
wasm-pack test --headless --chrome --no-default-features --features web-sys-webrtc
```

## Connection lifecycle and supported configuration

Retain the `ConnectionRef` returned by `new_connection` across asynchronous work.
Use `close_connection_if_current(&connection)` to retire only that physical
connection generation. `Pool::safely_remove_if_current` checks slot identity before
removal and physical close. Enumerate IDs with `connection_ids`, then resolve the
current reference with `connection`; do not assume a snapshot pins later lookups.
The CID-only close and connection-list APIs and the debug string stats API were
removed in issue #787. Structured core measurement remains a separate API.

ICE configuration supports password credentials only. The URL parser already
produces this credential type; deserializing `Oauth` now fails instead of allowing
native to fall back to password fields or mapping browser credentials differently.

The backend-free `--no-default-features` build has no normal Tokio dependency.
Its notifier and the default/dummy notifier use `native_timeout_scheduler`, which
works without an entered Tokio runtime. The native WebRTC backend still requires
Tokio for its send, close, and timer tasks.

Native sends use a one-shot close actor with exclusive state and physical-close
ownership. Caller and continuation report failures through a bounded mailbox only
after synchronous generation fencing. Pure reducers govern failure observations
and actor transitions; finite-state exploration and real actor conformance tests
check the ownership laws.
