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

## Milestones

- **Pre-1.0 (now).** The wire protocol is not versioned; a release that changes it is a
  network-wide upgrade, marked in the [CHANGELOG](./CHANGELOG.md).
- **1.0: a stable network.** The protocol is frozen once connectivity, churn handling, and
  convergence are stable.
- **2.0: DRanking.** Verifiable ranking of peers from service receipts
  ([paper](./papers/dranking.pdf)).

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
  and a browser/WASM provider; the hosted browser console at [rings.rs](https://rings.rs/#node).
- **Message admission** — at-most-once dispatch through a persisted per-origin replay window,
  per-origin rate limits at the destination, and delegations sent once per link and then
  referenced by content address (`crates/core/src/message`).
- **Bounded state** — connection admission bounded by the topology's reference slots, and DHT
  storage bounded in retention, element count, and bytes.

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
- Churn handling for 1.0: a churn simulator, successor lists sized from network estimates,
  RTT-derived liveness with ICE restart, inbox replication, stability-weighted storage, and
  browser lifecycle handling (#773–#779).

**Planned**
- Epoch-randomized ring positions, `pos(did, e) = H(did, beacon(e))`, so identities cannot
  choose where they land on the ring (#780).
- Decentralized peer discovery / bootstrapping.
- Richer routing primitives over the extension layer (pub/sub, service discovery as protocols).

---

## Privacy layer

> Goal: confidentiality and verifiability are defaults of the network, expressed as protocols
> over the same extension runtime.

**Shipped**
- Signed messaging with selectable signature schemes; plaintext and signed message paths.
- End-to-end ElGamal encryption to a DID's account key after the E2E handshake
  (`crates/core/src/message/e2e.rs`); opt-in, because a DID is a key digest and a lookup
  yields no key to encrypt to.
- Onion circuits over direct edges with layered ElGamal-AEAD frames, fixed-batch cover
  cells, pacing, and fixed cell size classes (`crates/node/src/onion`); the contract is drawn
  in [SECURITY.md](./SECURITY.md#layer-contracts).
- A path-less relay on the communication layer: the carrier names only the next hop, the
  destination, and a hop budget, so no hop learns the route
  (`crates/core/src/message/protocols/relay`).
- Persisted entry guards, and onion exits for TCP and HTTPS.
- Onion transport for applications: the native TUN gateway (`crates/gateway`) and the
  browser WebView that browses sites through circuits (`crates/webview`).

**In progress**
- Onion circuits as client-sealed loops of registered operation symbols (#834).

**Planned**
- User-installed zero-knowledge identity and verifiable off-chain compute protocols.
- Secret sharing and private storage primitives.
- DID-to-DID sender-unlinkable messaging on top of the circuits.

---

## Contributing to the roadmap

Direction is shaped through RFCs and issues on
[GitHub](https://github.com/RingsNetwork/rings/issues). If you want to help build either
layer, an extension protocol is the lightest way in — see **Extending Rings** in the
[README](./README.md).
