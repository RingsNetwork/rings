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
2. Peer B: { set received offer as remote description, create answer, set it as local description, Send Answer to Peer A }
3. Peer A: { Set received answer as remote description }

#### Network Layer

The Rings Network is a structured peer-to-peer network that incorporates a distributed hash table (DHT) to facilitate efficient and scalable lookups. The Chord algorithm is utilized to implement the lookup function within the DHT, thereby enabling effective routing of messages and storage of key-value pairs in a peer-to-peer setting. The use of a DHT, incorporating the Chord algorithm, guarantees high availability in the Rings Network, which is critical for handling the substantial number of nodes and requests typically present in large-scale peer-to-peer networks.

#### Privacy Layer

The network layer serves as the authenticated communication layer: it routes by DID using a signed `PayloadSender` envelope verified both hop-by-hop and end-to-end, and once two peers have exchanged account public keys through the E2E handshake, it encrypts application payloads end-to-end to the destination DID. Because a DID is the 160-bit digest of a public key, a DHT lookup yields an identifier to route to while keeping routing metadata minimal.

Circuit-level metadata privacy is provided by the dedicated privacy layer—the Sphinx/AEZ onion Kleisli pipelines in `crates/node/src/onion`: client-sealed loops of fixed-width Sphinx cells over direct edges, constant-rate link emission with real cells substituted for cover, paid admission, and fixed cell size classes. Each hop peels one cryptographic layer and learns only its immediate predecessor and successor; the exit processes the application payload without learning the client identity, and its reply returns through the client-sealed loop. The full guarantees of both layers are detailed in the [security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts) and the [Rings whitepaper](https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf).

#### Protocol and Identity Layer

Nodes address one another by 160-bit Decentralized Identifiers (DIDs) placed around the modular
ring `Z / 2^160 Z` (a finite commutative ring and cyclic additive group of order `2^160`, used for
Chord clockwise distance `(b - a) mod 2^160` and power-of-two finger offsets `(n + 2^k) mod 2^160`).
Account identities authorize short-lived secp256k1 delegatee session keys (`DelegateeKey`) through
signed `Delegation` proofs over secp256k1, EIP-191, BIP-137, secp256r1, Ed25519, or BLS12-381
accounts without exposing the account's long-term private key to the node.

#### Application Extension Layer

Upper-layer protocols are registered on a `Provider` as pure state machines (`Protocol::step`)
paired with namespace-isolated effect interpreters (`Interpret`). Inbound custom envelopes are
authenticated and replay-checked by `rings-core`, dispatched to their registered namespace, stepped
synchronously to produce `Effect` values, and interpreted over a namespace-scoped `Scope` capability
(overlay `send` and bounded self-feedback) with no cross-namespace state access.
