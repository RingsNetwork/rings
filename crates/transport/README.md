<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static.ringsnetwork.io/ringsnetwork_logo.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/asserts/main/logo/rings_network_red.png">
</picture>

# Rings Transport
======================

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings-node)


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
cargo build --features web-sys-webrtc --no-default-features
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

```
cargo test --features native-webrtc
```

```
cargo test --features web-sys-webrtc
```
