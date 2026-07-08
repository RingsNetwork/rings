# Rings Network — Roadmap

## Vision

A **fully server-less, decentralized sovereign network**: every node — a daemon or a plain
browser tab — participates directly, and no centralized infrastructure sits in the data path.
Two tracks carry the work:

- **Network layer** — connectivity, routing and transport with no servers to depend on.
- **Privacy layer** — confidentiality and verifiable computation by default, not as an add-on.

This document tracks what is shipped versus where each track is heading. Items under
*In progress* / *Planned* are direction, not commitments, and are refined as RFCs land.

---

## Foundations (shipped)

The substrate both layers build on:

- **Chord DHT** routing — successor/finger tables, stabilization, DID addressing
  (`crates/core`).
- **WebRTC transport**, native and browser (`web_sys`), with STUN/ICE/SDP NAT traversal and
  direct peer-to-peer datachannels (`crates/transport`).
- **DID identity** with secp256k1 / secp256r1 / ed25519 / BLS / bip137 signatures
  (`crates/core::ecc`).
- **`network_id`-isolated overlays**, so independent networks don't intermix.
- **Extension/protocol model** — pure `Protocol` + namespace-scoped `Interpret` shells, with
  inbound envelopes routed by namespace (RFC #594; `crates/node/src/extension`).
- **Control & embedding surfaces** — native daemon + `rings` CLI, JSON-RPC over HTTP, C FFI,
  and a browser/WASM provider.

---

## Network layer

> Goal: remove every centralized dependency from connectivity — discovery, NAT traversal and
> routing all run between peers.

**Shipped**
- Browser ↔ browser direct datachannels (no media/relay server in the path).
- Overlay message relay (DHT-routed delivery between peers that aren't directly connected).
- Built-in **relay protocol**: tunnel a local TCP/UDP socket to a peer's service across the
  overlay — server-less tunneling and peer-exit (`node::extension::protocols::relay`).

**In progress**
- WebTransport-backed relay in the browser (compile-checked; runtime hardening).
- Connection resilience / optimistic send on the overlay.

**Planned**
- Decentralized peer discovery / bootstrapping.
- Richer routing primitives over the extension layer (pub/sub, service discovery as protocols).

---

## Privacy layer

> Goal: confidentiality and verifiability are defaults of the network, expressed as protocols
> over the same extension runtime.

**Shipped**
- **zkSNARK** proving/verification as a protocol — fold-scheme based (`crates/snark`).
- Signed messaging with selectable signature schemes; plaintext and signed message paths.

**Planned**
- End-to-end encryption across the messaging layer (sender-to-recipient, not hop-by-hop).
- Zero-knowledge identity and verifiable off-chain compute built on the SNARK protocol.
- Secret sharing and private storage primitives.
- Metadata-resistant routing (reducing what intermediaries can observe).

---

## Contributing to the roadmap

Direction is shaped through RFCs and issues on
[GitHub](https://github.com/RingsNetwork/rings/issues). If you want to help build either
layer, an extension protocol is the lightest way in — see **Extending Rings** in the
[README](./README.md).
