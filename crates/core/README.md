<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static.ringsnetwork.io/ringsnetwork_logo.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/asserts/main/logo/rings_network_red.png">
</picture>


Rings Core
===============

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-core.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-core/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)


# Architecture

The Rings Network architecture is streamlined into five distinct layers.

#### Runtime Layer

The design goal of Rings Network is to enable nodes to run in any environment, including browsers, mobile devices, Linux, Mac, Windows, etc. To achieve this, we have adopted a cross platform compile approach to build the code. For non-browser environments, we use pure Rust implementation that is independent of any system APIs, making our native implementation system agnostic. For browser environments, we compile the pure Rust implementation to WebAssembly (Wasm) and use web-sys, js-sys, and wasm-bindgen to glue it together, making our nodes fully functional in the browser.

#### Transport Layer

The transport layer of Rings Network is based on WebRTC protocol, which provides browsers and mobile apps with real-time communication capabilities through simple interface.

The WebRTC protocol obtains the optimal connection path between nodes by exchanging SDP (Session Description Protocol), which can be either TCP or UDP. In the Rings Network, we use WebRTC's data channel to implement data communication. For a typical ICE (Interactive Connectivity Establishment) process, it can be described as follows:

Assuming Node A and Node B want to create a WebRTC connection, they would need to exchange a minimum of three messages with each other:

**ICE Scheme:**

1. Peer A: { create offer, set it as local description } -> Send Offer to Peer B
2. Peer B: { set receiveed offer as remote description create answer set it as local description Send Answer to Peer A }
3. Peer A: { Set receiveed answer as remote description }

For native implementation, the transport layer is based on `webrtc.rs`, and for browser case, we implemented based on `web_sys` and `wasm_bindgen`.

To check the implementation of transport layer: https://github.com/RingsNetwork/rings-node/tree/master/transport


#### Network Layer

The network layer is the core component of the Rings Network, responsible for DID (Decentralized Identifier) discovery and services routing within the network. The Rings Network employs the Chord DHT (Distributed Hash Table) algorithm as the implementation for its network layer.

The Chord algorithm is a well-known DHT algorithm characterized by its formation of an abstract circular topology structure among all participating nodes. It has a lookup algorithm complexity of O(log(N)).

#### Protocol Layer

In the protocol layer, the central design concept revolves around the utilization of a Decentralized Identifier (DID), which constitutes a finite ring in abstract algebra. The DID is a 160-bit identifier that enables the construction of a mathematical structure that encompasses the characteristics of both a group and a field.

It is comprised of a set of elements with two binary operations, addition and multiplication, which satisfy a set of axioms such as associativity, commutativity, and distributivity. The ring is deemed finite due to its having a finite number of elements. Finite rings are widely employed in various domains of mathematics and computer science, including cryptography and coding theory.

At the protocol layer, we have implemented the concept of a Delegated Session Key, which is used to support various cryptographic verification methods associated with DID (Decentralized Identifier). Currently, the supported signature algorithms include ECDSA-secp256k1, ECDSA-secp256r1, and EdDSA-ed25519.

#### Application Layer

The nucleus of Rings Network is similar to the Actor Model, and it requires that each message type possess a Handler Trait. This allows for the separation of processing system messages, network messages, internal messages, and application-layer messages.


# Build

#### Build for native

```
cargo build
```

#### Build for wasm

```
cargo build --features wasm --no-default-features
```
