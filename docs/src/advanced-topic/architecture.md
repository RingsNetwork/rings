# Architecture

The Rings Network architecture is organized into six decoupled layers across native, browser
(WebAssembly), and C FFI targets.

#### Runtime Layer

The design goal of Rings Network is to enable nodes to run across browsers, desktop/server OSes
(Linux, macOS, Windows), and host languages via C FFI. Platform task execution (`Spawner`,
`TaskSet`, timers, and `MaybeSendSync`) is isolated inside `crates/runtime` over Tokio on native
targets and `wasm-bindgen-futures` / `web-sys` / `js-sys` in browsers, leaving `rings-core`
independent of specific OS executors or WebRTC libraries.

#### Transport Layer

Peer connectivity is decoupled from `rings-core` behind `TransportInterface`,
`ConnectionInterface`, and `NetworkConfigDescriptor` (`crates/transport` and
`network: { kind, params }` in `config.yaml`). The default backend uses WebRTC (`webrtc = 0.17.3`
on native, `RTCPeerConnection` in browsers) with STUN, UDP/TCP TURN (the built-in `turn_tcp` TURN-over-TCP/TLS bridge over TCP/TLS
for UDP-blocked networks), and a generalized `0`–`8` step binary `HandshakeStep` exchange. Every
link multiplexes four ordered, credit-flow-controlled channels, and wire frames use Postcard
(`rings-codec`, `WIRE_VERSION = 6`) with bounded static-dictionary LZ4 compression for DHT control
and storage payloads. See [Transport Abstraction](transport.md) and
[Wire Protocol & LZ4 Compression](wire-protocol.md).

#### Network Layer

Rings is a structured peer-to-peer network built on a range-proved Chord DHT (`crates/core`):
`K = 4` successor lists, 160-bit finger tables, non-transitive multi-predecessor & passive
gap-bridge hints, voluntary `Leave` with carrier handoff, authenticated CRDT data topics,
chain-replicated relay inboxes, and destination-scoped transaction replay protection.

#### Privacy Layer

The network layer serves as the authenticated communication layer: it routes by DID using signed
envelopes verified hop-by-hop and end-to-end, and once two peers exchange account public keys
through the E2E handshake, it encrypts application payloads end-to-end (ECC ElGamal) to the
destination DID.

Circuit-level metadata privacy is provided by the dedicated privacy layer (`crates/node/src/onion`
and `crates/aez`)—guard-closed Sphinx/AEZ v5 loops over direct edges, dormant-by-default outbound
lanes (`OnionIdleFloor::OFF`, `#1314`) with active-window constant-rate slot pacing (substituting
real cells for cover, plus configurable background cover floors), pinned entry guards, paid
`SlotRoster` admission, fixed cell size classes, world-facing `tcp`/`https`/`udp` exits (powering
HTTP `CONNECT`, the selective IPv4/TCP TUN gateway, Onion WebView, and multi-hop Onion TURN), and
composable Kleisli `invoke` pipelines. See [Onion Routing & Kleisli Pipelines](../onion.md), the
[security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts),
and the [Rings whitepaper](https://rings.rs/papers/rings/).

#### Protocol and Identity Layer

Nodes address one another by 160-bit Decentralized Identifiers (DIDs) placed around the modular
ring `Z / 2^160 Z` (used for Chord clockwise distance `(b - a) mod 2^160` and power-of-two finger
offsets `(n + 2^k) mod 2^160`). Account identities authorize short-lived secp256k1 delegatee
session keys (`DelegateeKey`) through signed `Delegation` proofs over secp256k1, EIP-191, BIP-137,
secp256r1, Ed25519, or BLS12-381 accounts without exposing the account's long-term private key to
the node.

#### Application Extension Layer

Upper-layer protocols are registered on a `Provider` as pure state machines (`Protocol::step`)
paired with namespace-isolated effect interpreters (`Interpret`). Inbound custom envelopes are
authenticated and replay-checked by `rings-core`, dispatched to their registered namespace, stepped
synchronously to produce `Effect` values, and interpreted over a namespace-scoped `Scope` capability
(overlay `send` and bounded self-feedback) with no cross-namespace state access.

## Workspace Crate Map

The repository workspace contains 18 crates (16 published to crates.io plus 2 internal crates):

| Crate | Package | Role |
|---|---|---|
| `crates/aez` | `rings-aez` | AEZ v5 wide-block cipher for fixed-width onion cell layers |
| `crates/codec` | `rings-codec` | Canonical Postcard wire codec with trailing-byte rejection |
| `crates/core` | `rings-core` | Chord DHT, CRDT storage, delegation, replay/quotas, E2E, swarm |
| `crates/derive` | `rings-derive` | Procedural macros (`#[wasm_export]`, `wasm` feature) |
| `crates/dns-relay` | `rings-dns-relay` | Authoritative `<did>.<zone>` DNS server + IPv6 TCP ingress relay |
| `crates/gateway` | `rings-gateway` | Selective IPv4/TCP TUN gateway (`smoltcp`) + `gateway-config-unix` helper |
| `crates/logo` | `rings-logo` | Internal SVG/PNG branding asset generator (`publish = false`) |
| `crates/measure` | `rings-measure` | Pure local aMule credit, reliability windows, provisional evidence store |
| `crates/network-policy` | `rings-network-policy` | Shared public-IP / public-host egress predicates (SSRF defense) |
| `crates/node` | `rings-node` | Full node daemon, `rings` CLI, onion stack, mail, C FFI, Browser WASM `Provider` |
| `crates/rpc` | `rings-rpc` | JSON-RPC 2.0 `Method` catalog, authorization classes, DTOs, HTTP client |
| `crates/runtime` | `rings-runtime` | Native (`tokio`) and browser WASM async executor & timer contracts |
| `crates/test-support` | `rings-test-support` | Internal deterministic test utilities (`publish = false`) |
| `crates/transport` | `rings-transport` | Decoupled transport traits, N-step handshake, WebRTC, TURN-over-TCP/TLS bridge |
| `crates/tui` | `rings-tui` | Interactive 6-tab Ratatui terminal operator workbench (`rings-tui`) |
| `crates/turn` | `rings-turn` | Signature- and ECDH-authenticated UDP/TCP RFC 5766 TURN server (`rings-turn`) |
| `crates/turn-service` | `rings-turn-service` | TURN DHT discovery (`turn_services`), `turn.ticket` invoke, multi-hop `onion::udp` bridge |
| `crates/webview` | `rings-webview` | Reusable Onion WebView gateway primitives & opaque-origin CSP sandbox policy |
