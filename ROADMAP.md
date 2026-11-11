# Rings Network — Roadmap

Goal: a fully server-less network where every node, daemon or browser tab, participates
directly. *In progress* and *Planned* items are direction, not commitments.

## Milestones

- **1.0: a stable network.** Delivers deterministic wire-versioned framing
  (`"RINGS\0" ‖ wire_version`, `WIRE_VERSION = 3`), range-proved and non-transitive Chord
  stabilization, CRDT removal stability, durable group-committed local stores and chain-replicated
  relay custody, fair slot-isolated admission and load bounds, and client-sealed Sphinx onion
  Kleisli pipelines (#834, #843, #880). Stabilizes the `1.0.x` public runtime contracts across the
  `1.0.x` release line; see [UPGRADING.md](./UPGRADING.md) and the unified
  [Rings whitepaper](./papers/rings.pdf).
- **2.0: DRanking & randomized topology.** Verifiable peer ranking from cryptographic service
  receipts ([paper](./papers/dranking.pdf)) and epoch-randomized ring placement (#780).

## Shipped

- **Overlay:** Chord DHT with DID addressing, `network_id` isolation, range-proved finger
  convergence, non-transitive reachability healing, and a path-less relay whose carrier names only
  the next hop, destination, and hop budget (`crates/core`); connect signaling records the relays
  it crosses so its answer retraces them directly (#947).
- **Transport:** WebRTC datachannels across native and browser peers, browser-to-browser included,
  with 4-lane credit flow control and independent challenge-response liveness probes
  (`crates/transport`).
- **Identity:** DIDs over secp256k1, secp256r1, ed25519, BLS, and bip137 keys; receiver-leased
  delegation references and E2E ElGamal encryption to a DID after its handshake.
- **Admission & storage:** at-most-once delivery through a group-committed per-origin replay
  window, slot-isolated resource accounting (`SlotRoster` / `TableAccounting`), per-origin rate
  quotas, clock-bounded CRDT removal stability, and chain-replicated relay-inbox custody.
- **Privacy:** client-sealed Sphinx/AEZ onion Kleisli pipelines (#834, #843, #880) over fixed-width
  cells, constant-rate link emission with real cells substituted for cover, paid admission, size
  classes, persisted entry guards, and TCP/HTTPS exits (`crates/node/src/onion`); the TUN gateway
  (`crates/gateway`) and onion WebView (`crates/webview`).
- **Runtimes:** native daemon and `rings` CLI, JSON-RPC, C FFI, the browser provider, and the
  hosted console at [rings.rs](https://rings.rs/#node).
- **Protocols:** pure `Protocol` + namespace-scoped `Interpret` shells (RFC #594), with
  built-in TCP/UDP relay tunnels, DNS relay, and SMTP/IMAP mail gateway.

## In progress

- Adaptive churn telemetry and tuning (#773–#779): churn simulation harnesses, dynamic successor-list
  sizing from network estimates, RTT-aware ICE restart tuning, and browser lifecycle refinements.

## Planned

- Epoch-randomized ring positions, `pos(did, e) = H(did, beacon(e))`, so identities cannot
  choose where they land (#780).
- Decentralized peer discovery and bootstrapping.
- Pub/sub and service discovery as protocols.
- DID-to-DID sender-unlinkable messaging over the loops.
- Zero-knowledge identity, verifiable off-chain compute, secret sharing, and private storage
  as user-installed protocols.

Direction is shaped through [issues](https://github.com/RingsNetwork/rings/issues). A
protocol is the lightest way to contribute; see **Build on Rings** in the [README](./README.md).
