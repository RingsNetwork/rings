# Rings Network — Roadmap

Goal: a fully server-less network where every node, daemon or browser tab, participates
directly. *In progress* and *Planned* items are direction, not commitments.

## Milestones

- **Pre-1.0 (now).** The wire protocol is not versioned; a release that changes it is a
  network-wide upgrade, marked in the [CHANGELOG](./CHANGELOG.md).
- **1.0: a stable network.** The protocol freezes once connectivity, churn handling, and
  convergence are stable.
- **2.0: DRanking.** Verifiable peer ranking from service receipts
  ([paper](./papers/dranking.pdf)).

## Shipped

- **Overlay:** Chord DHT with DID addressing, `network_id` isolation, and a path-less relay
  whose carrier names only the next hop, destination, and hop budget (`crates/core`).
- **Transport:** WebRTC datachannels, native and browser, browser to browser included
  (`crates/transport`).
- **Identity:** DIDs over secp256k1, secp256r1, ed25519, BLS, and bip137 keys; E2E ElGamal
  encryption to a DID after its handshake.
- **Admission:** at-most-once delivery through a persisted per-origin replay window,
  per-origin rate limits, delegations referenced by content address, and bounded
  connection and storage state.
- **Privacy:** onion circuits with layered ElGamal-AEAD, fixed-batch cover, pacing, size
  classes, persisted entry guards, and TCP/HTTPS exits (`crates/node/src/onion`); the TUN
  gateway (`crates/gateway`) and onion WebView (`crates/webview`).
- **Runtimes:** native daemon and `rings` CLI, JSON-RPC, C FFI, the browser provider, and the
  hosted console at [rings.rs](https://rings.rs/#node).
- **Protocols:** pure `Protocol` + namespace-scoped `Interpret` shells (RFC #594), with
  built-in TCP/UDP relay tunnels.

## In progress

- Churn handling for 1.0 (#773–#779): a churn simulator, successor lists sized from network
  estimates, RTT-derived liveness with ICE restart, inbox replication, stability-weighted
  storage, and browser lifecycle handling.
- Onion circuits as client-sealed loops of registered operation symbols (#834).
- WebTransport-backed relay in the browser.

## Planned

- Epoch-randomized ring positions, `pos(did, e) = H(did, beacon(e))`, so identities cannot
  choose where they land (#780).
- Decentralized peer discovery and bootstrapping.
- Pub/sub and service discovery as protocols.
- DID-to-DID sender-unlinkable messaging over circuits.
- Zero-knowledge identity, verifiable off-chain compute, secret sharing, and private storage
  as user-installed protocols.

Direction is shaped through [issues](https://github.com/RingsNetwork/rings/issues). A
protocol is the lightest way to contribute; see **Build on Rings** in the [README](./README.md).
