# Architecture

The Rings Network architecture is streamlined into six distinct layers.

#### Runtime Layer

The design goal of Rings Network is to enable nodes to run in any environment, including browsers, mobile devices, Linux, Mac, Windows, etc. To achieve this, we have adopted a cross platform compile approach to build the code. For non-browser environments, we use pure Rust implementation that is independent of any system APIs, making our native implementation system agnostic. For browser environments, we compile the pure Rust implementation to WebAssembly (Wasm) and use web-sys, js-sys, and wasm-bindgen to glue it together, making our nodes fully functional in the browser.

#### Transport Layer

The Transport implementation in Rings Network is based on WebRTC, making Rings Network browser-native.

The browser-native approach means that users do not need to install any additional software or plugins to participate in the network. Furthermore, the use of WebRTC and WebAssembly enables the network to achieve low latency and high throughput, making it suitable for real-time communication and data transfer.

Assuming Node A and Node B want to create a WebRTC connection, they would need to exchange a minimum of three messages with each other:

**ICE Scheme:**

1. Peer A: { create offer, set it as local description } -> Send Offer to Peer B
2. Peer B: { set receiveed offer as remote description create answer set it as local description Send Answer to Peer A }
3. Peer A: { Set receiveed answer as remote description }

#### Network Layer

The Rings Network is a structured peer-to-peer network that incorporates a distributed hash table (DHT) to facilitate efficient and scalable lookups. The Chord algorithm is utilized to implement the lookup function within the DHT, thereby enabling effective routing of messages and storage of key-value pairs in a peer-to-peer setting. The use of a DHT, incorporating the Chord algorithm, guarantees high availability in the Rings Network, which is critical for handling the substantial number of nodes and requests typically present in large-scale peer-to-peer networks.

#### Privacy Layer

The network layer is the communication layer: it routes by DID and, once two peers have exchanged account public keys through the E2E handshake, encrypts a payload to its destination. A DID is the 160-bit digest of a public key, so a lookup yields an identifier to route to and not a key to encrypt to, and every hop sees the origin DID by signature and the destination DID by routing. The communication layer therefore minimizes what it leaks and does not provide privacy.

Privacy is provided by the privacy layer, the onion circuits in `crates/node/src/onion`: layered ElGamal-AEAD frames over direct edges, fixed-batch cover cells with pacing, and fixed cell size classes. Each relay decrypts one layer and learns only its predecessor and its successor; the exit sees the application payload but not the client. The contract of each layer, including what circuits do not hide, is drawn in the [security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts).

#### Protocol Layer

In the protocol layer, the central design concept revolves around the utilization of a Decentralized Identifier (DID), which constitutes a finite ring in abstract algebra. The DID is a identifier that enables the construction of a mathematical structure that encompasses the characteristics of both a group and a field. It is comprised of a set of elements with two binary operations, addition and multiplication, which satisfy a set of axioms such as associativity, commutativity, and distributivity. The ring is deemed finite due to its having a finite number of elements. Finite rings are widely employed in various domains of mathematics and computer science, including cryptography and coding theory.

#### Application Layer

The nucleus of Rings Network is similar to the Actor Model, and it requires that each message type possess a Handler Trait. This allows for the separation of processing system messages, network messages, internal messages, and application-layer messages.
