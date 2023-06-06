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

The implementation of WebRTC and WebAssembly in Rings Network provides several advantages for users. Firstly, the browser-based approach means that users do not need to install any additional software or plugins to participate in the network. Secondly, the use of WebRTC and WebAssembly enables the network to have a low latency and high throughput, making it suitable for real-time communication and data transfer.

WebRTC, or Web Real-Time Communication, provides browsers and mobile apps with real-time communication capabilities through simple APIs. With WebRTC, users can easily send audio, video, and data streams directly between browsers, without the need for any plug-ins or extra software. At the same time, the Rings Network has some special optimizations for the WebRTC handshakes process.

Assuming Node A and Node B want to create a WebRTC connection, they would need to exchange a minimum of three messages with each other:



**ICE Scheme:**

1. Peer A: { create offer, set it as local description } -> Send Offer to Peer B
2. Peer B: { set receiveed offer as remote description create answer set it as local description Send Answer to Peer A }
3. Peer A: { Set receiveed answer as remote description }

#### Network Layer

The Rings Network is a structured peer-to-peer network that incorporates a distributed hash table (DHT) to facilitate efficient and scalable lookups. The Chord algorithm is utilized to implement the lookup function within the DHT, thereby enabling effective routing of messages and storage of key-value pairs in a peer-to-peer setting. The use of a DHT, incorporating the Chord algorithm, guarantees high availability in the Rings Network, which is critical for handling the substantial number of nodes and requests typically present in large-scale peer-to-peer networks.

#### Protocol Layer

In the protocol layer, the central design concept revolves around the utilization of a Decentralized Identifier (DID), which constitutes a finite ring in abstract algebra. The DID is a

\-bit identifier that enables the construction of a mathematical structure that encompasses the characteristics of both a group and a field. It is comprised of a set of elements with two binary operations, addition and multiplication, which satisfy a set of axioms such as associativity, commutativity, and distributivity. The ring is deemed finite due to its having a finite number of elements. Finite rings are widely employed in various domains of mathematics and computer science, including cryptography and coding theory.

#### Application Layer

The nucleus of Rings Network is similar to the Actor Model, and it requires that each message type possess a Handler Trait. This allows for the separation of processing system messages, network messages, internal messages, and application-layer messages.
