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

* rpc: The definition of Rings RPC protocol is here.

* derive: Rings macros, including `wasm_export` macro.

* transport: Rings Transport implementation, including native transport and `web_sys` based transport.

* snark: Rings SNARK is based on fold scheme and zkSNARK

## Architecture

The Rings Network architecture is streamlined into five distinct layers.

```text
+-----------------------------------------------------------------------------------------+
|                                         RINGS                                           |
+-----------------------------------------------------------------------------------------+
|   Encrypted IM / Secret Sharing Storage / Distributed Content / Secret Data Exchange    |
+-----------------------------------------------------------------------------------------+
|                  SSSS                  |  Perdson Commitment/zkPod/ Secret Sharing      |
+-----------------------------------------------------------------------------------------+
|                     |       dDNS       |                 Sigma Protocol                 |
+      K-V Storage    +------------------+------------------------------------------------+
|                     |  Peer LOOKUP     |          MSRP        |  End-to-End Encryption  |
+----------------------------------------+------------------------------------------------+
|            Peer-to-Peer Network        |                                                |
|----------------------------------------+               DID / Resource ID                |
|                 Chord DHT              |                                                |
+----------------------------------------+------------------------------------------------+
|                Trickle SDP             |        ElGamal        | Persistence Storage    |
+----------------------------------------+-----------------------+------------------------+
|            STUN  | SDP  | ICE          |  Crosschain Binding   | Smart Contract Binding |
+----------------------------------------+------------------------------------------------+
|             SCTP | UDP | TCP           |             Finate Pubkey Group                |
+----------------------------------------+------------------------------------------------+
|   WASM Runtime    |                    |                                                |
+-------------------|  Operation System  |                   ECDSA                        |
|     Browser       |                    |                                                |
+-----------------------------------------------------------------------------------------+
```

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
