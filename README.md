<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static.ringsnetwork.io/ringsnetwork_logo.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/asserts/main/logo/rings_network_red.png">
</picture>

Rings Network
===============

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings-node)
[![Sponsor](https://img.shields.io/badge/Sponsor-RingsNetwork-ea4aaa?logo=githubsponsors)](https://github.com/sponsors/RingsNetwork)

The Rings Network aimed at creating a fully decentralized network. It is built upon technologies such as WebRTC, WASM (WebAssembly), and Chord DHT (Distributed Hash Table), enabling direct connections between browsers.

Rings Network allows all traffic to bypass centralized infrastructures, achieving complete decentralization.


For more details you can check our [Rings Whitepaper](https://raw.githubusercontent.com/RingsNetwork/whitepaper/master/rings.pdf).

You can also visit [Rings Network's homepage](https://ringsnetwork.io) to get more project info.

And you can get more document [here](https://rings.gitbook.io/).


## Features

### Browser Native:

Utilizing WebRTC, a protocol designed for real-time communication, the Rings Network is fully compatible with browser environments. This capability is further enhanced by their full Rust implementation and web_sys based approach, enabling seamless, direct browser-to-browser communication.

### Crypto Native:

A core aspect of the Rings Network is its support for various cryptographic algorithms, essential for DID (Decentralized Identifier) identification. This includes support for popular cryptographic standards like secp256k1, secp256r1, and ed25519, among others, providing robust security and identity verification mechanisms.

### Struct P2P:

At the foundation of the Rings Network is the use of Chord DHT (Distributed Hash Table). This technology underpins the routing layer of the network, enabling efficient, scalable, and decentralized peer-to-peer connectivity. The use of Chord DHT ensures that the network can handle a large number of nodes while maintaining effective data retrieval and communication processes.

## Installation

You can install rings-node either from Cargo or from source.

### from cargo

To install rings-node from Cargo, run the following command:

```sh
cargo install rings-node
```

### from source

To install rings-node from source, follow these steps:

```sh
git clone git@github.com:RingsNetwork/rings-node.git
cd ./rings-node
cargo install --path .
```

### Build for WebAssembly


To build Rings Network for WebAssembly, run the following commands:

```sh
cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm
```

Or build with `wasm-pack`

```sh
wasm-pack build --scope ringsnetwork -t web --no-default-features --features browser --features console_error_panic_hook
```


## Usage

```sh
rings help
```

## Examples

Runnable examples live in [`examples/`](./examples):

| Example | What it shows |
|---|---|
| [`native`](./examples/native) | A minimal native node registering a custom namespaced protocol |
| [`browser`](./examples/browser) | The same protocol model in the browser (WASM) — same Rust code, no JS handler |
| [`relay`](./examples/relay) | TCP & UDP tunnels to a peer's service over the overlay (`tcp.rs` / `udp.rs`) |
| [`snark`](./examples/snark) | Fold-scheme zkSNARK proving / verification |
| [`proof-demo`](./examples/proof-demo) | A browser zk-proof app (Yew / Trunk) |
| [`dweb`](./examples/dweb) | A decentralized-web app (Yew / Trunk) |
| [`ffi`](./examples/ffi) | Driving a node over the C FFI |

## Extending Rings

A protocol is a **pure** state machine; all IO lives in its interpreter shell, which can only
act within its own namespace. Inbound overlay messages are routed to a protocol by namespace.

```rust
// Register a pure Protocol + its Interpret shell, then route inbound envelopes to it.
provider.register_protocol(Echo, EchoShell)?;
provider.set_backend()?;

// Built-in relay: tunnel a local socket to a peer's service over the overlay — no server.
let relay = RelayHandle::install(&provider.extensions())?;
relay.register_tcp_service("web".into(), "example.com:80".parse()?).await?; // server side
relay.open_tcp_tunnel(local_addr, peer_did, "web".into()).await?;          // client side
```

In the browser a protocol can be a JS handler instead: `provider.on(namespace, initialState,
handler)`. See [`examples/relay`](./examples/relay) and
[`crates/node/src/extension`](./crates/node/src/extension).

## Resource

| Resource                         | Link                                                                       | Status                                                                                                                                                                                    |
|----------------------------------|----------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Rings Whitepaper                 | [Rings Whitepaper](https://github.com/RingsNetwork/whitepaper)             | [![rings-ext-v2](https://github.com/RingsNetwork/rings_ext_v2/actions/workflows/dev.yml/badge.svg)](https://github.com/RingsNetwork/rings_ext_v2/actions/workflows/dev.yml)               |
| Rings Documentation              | [Rings Docs](https://rings.gitbook.io/)                                    |                                                                                                                                                                                           |
| Rings Browser Handshakes Example | [Rings Browser Handshakes](https://github.com/RingsNetwork/rings-wasm-p2p) | Demo / PoC                                                                                                                                                                                |
| Rings Browser Extension          | [Rings Browser Extension](https://github.com/RingsNetwork/rings_ext_v2)    | Beta                                                                                                                                                                                      |
| Rings dWeb Demo                  | [Rings dWeb Demo](https://github.com/RingsNetwork/rings-dweb)              | [![rings-ext-v2](https://github.com/RingsNetwork/rings_dweb/actions/workflows/nextjs.yml/badge.svg?branch=page)](https://github.com/RingsNetwork/rings_dweb/actions/workflows/nextjs.yml) |
|Rings zkProof Demo             | [Rings zkProof Demo](https://zkp.rings.rs)  |![rings-snark-demo](https://github.com/RingsNetwork/rings-proof-demo/actions/workflows/nextjs.yml/badge.svg?branch=page)|

## Components:

* core: The core implementation of rings network, including DHT and Swarm.

* node: The implementation of Rings native, Rings browser, and Rings FFI provider.

* rpc: Rings RPC shared types and the JSON-RPC client/handlers (over HTTP).

* derive: Rings macros, including `wasm_export` macro.

* transport: Rings Transport implementation, including native transport and `web_sys` based transport.

* snark: Rings SNARK is based on fold scheme and zkSNARK

## Architecture

Rings is layered so that **every layer is decentralized — there is no server in the data
path**. Each layer maps directly to a crate/module:

```text
┌──────────────────────────────────────────────────────────────────────┐
│  Applications   dWeb · zk-proof demo · relay/tunnel · your own app     │
├──────────────────────────────────────────────────────────────────────┤
│  Protocols      built-ins: relay (tcp/udp tunnels), SNARK, echo —      │  node::extension::protocols
│  (namespaced)   plus any user Protocol, addressed by namespace         │  crates/snark
├──────────────────────────────────────────────────────────────────────┤
│  Extension      pure `Protocol::step` → `Effect` → `Interpret` shell   │  node::extension::ext
│  runtime        over a namespace-scoped `Scope` (send / self-inject)   │
├──────────────────────────────────────────────────────────────────────┤
│  Overlay        Chord DHT: successor / finger tables, stabilization,   │  crates/core
│  (routing)      DID addressing, message relay, network_id isolation    │
├──────────────────────────────────────────────────────────────────────┤
│  Transport      direct WebRTC datachannels (native + browser/web_sys), │  crates/transport
│                 STUN / ICE / SDP NAT traversal                         │
├──────────────────────────────────────────────────────────────────────┤
│  Identity       DID + secp256k1 / secp256r1 / ed25519 / BLS / bip137   │  crates/core::ecc
└──────────────────────────────────────────────────────────────────────┘
```

- **Transport** establishes direct, peer-to-peer WebRTC datachannels — browser-to-browser
  included — using STUN/ICE/SDP for NAT traversal, so traffic never transits a central server.
- **Overlay** organizes peers into a Chord DHT and routes messages by DID; distinct overlays are
  isolated by `network_id`.
- **Extension runtime** is a *functional core / imperative shell*: a protocol's state transition
  is pure (`step`), and all IO happens in its `Interpret` shell, which only ever receives a
  **namespace-scoped capability** (`Scope`). The core owns no global effect/command bus — adding
  a protocol never touches it, and a protocol cannot reach another namespace.
- **Protocols** are addressed by namespace. Built-ins include a **relay** that tunnels local
  TCP/UDP sockets to a peer's service across the overlay (server-less tunneling / peer exit), and
  **SNARK** (fold-scheme zkSNARK) proving/verification. Register your own with
  `provider.register_protocol(..)` (Rust) or `provider.on(namespace, ..)` (JS).

Where this is heading — a fully server-less, sovereign **network layer** and **privacy layer** —
is described in [ROADMAP.md](./ROADMAP.md).

## Contributing

We welcome contributions to rings-node!

If you have a bug report or feature request, please open an issue on GitHub.

If you'd like to contribute code, please follow these steps:

```text
    Fork the repository on GitHub.
    Create a new branch for your changes.
    Make your changes and commit them with descriptive commit messages.
    Push your changes to your fork.
    Create a pull request from your branch to the main repository.
```

We'll review your pull request as soon as we can, and we appreciate your contributions!


## Ref:

1. <https://datatracker.ietf.org/doc/html/rfc5245>

2. <https://datatracker.ietf.org/doc/html/draft-ietf-rtcweb-ip-handling-01>

3. <https://datatracker.ietf.org/doc/html/rfc8831>

4. <https://datatracker.ietf.org/doc/html/rfc8832>
