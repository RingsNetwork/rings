# Rings Network — Roadmap

Goal: a fully server-less network where every node, daemon or browser tab, participates
directly. *In progress* and *Planned* items are direction, not commitments.

## Milestones

- **1.0: a stable network.** Delivers deterministic wire-versioned framing
  (`"RINGS\0" ‖ wire_version`, `WIRE_VERSION = 6`), range-proved and non-transitive Chord
  stabilization, CRDT removal stability, durable group-committed local stores and chain-replicated
  relay custody, fair slot-isolated admission and load bounds, and client-sealed Sphinx onion
  Kleisli pipelines (#834, #843, #880). Stabilizes the `1.0.x` public runtime contracts across the
  `1.0.x` release line; see [UPGRADING.md](./UPGRADING.md) and the unified
  [Rings whitepaper](https://rings.rs/papers/rings/).
- **2.0: DRanking & randomized topology.** Verifiable peer ranking from cryptographic service
  receipts ([paper](https://rings.rs/papers/dranking/)) and epoch-randomized ring placement (#780).

## Shipped

- **Overlay:** Chord DHT with DID addressing, `network_id` isolation, range-proved finger
  convergence, non-transitive multi-predecessor & passive gap-bridge routing, voluntary `Leave`
  with ack-gated carrier handoff (#1057), and a path-less relay whose carrier names only
  the next hop, destination, and hop budget (`crates/core`); connect signaling records the relays
  it crosses so its answer retraces them directly (#947).
- **Transport:** decoupled `TransportInterface` and `ConnectionInterface` (#1291) with 0–8 step
  binary `HandshakeStep` negotiation, WebRTC datachannels across native and browser peers
  (`webrtc = 0.17.3` + `web_sys`), built-in TURN-over-TCP & TURNS bridge (the built-in `turn_tcp` TURN-over-TCP/TLS bridge, #1287),
  quiet-window ICE settling (`#1288`), bounded static-dictionary LZ4 wire compression and compact
  `WirePayload` encoding at `WIRE_VERSION = 6` (`480 B` max chunk envelope overhead, down from
  `742 B`; `#1280`, `#1283`, `#1315`), 4-lane credit flow control, and independent liveness probes
  (`crates/transport`).
- **TURN & Onion TURN:** standalone stateless signature+ECDH RFC 5766 UDP/TCP TURN server
  (`rings-turn`) plus `rings-turn-service` publishing `TurnServiceDescriptor` to `turn_services`,
  issuing unlinkable `SessionTicket` credentials over `turn.ticket` onion `invoke`, and
  bridging allocations through split-trust multi-hop `onion::udp` circuits (`#1278`, `#1292`).
- **Identity:** DIDs over secp256k1 (EIP-191, bip137), secp256r1, ed25519, and BLS12-381 keys;
  receiver-leased delegation references, in-process `delegator_key` rotation, and E2E ElGamal
  encryption to a DID after its handshake.
- **Admission & storage:** at-most-once delivery through a group-committed per-origin replay
  window, slot-isolated resource accounting (`SlotRoster` / `TableAccounting`), per-origin rate
  quotas, actor-isolated Merkle-authenticated CRDT data topics (`SignedDataMutation`, `#1179`, `#1220`, `#1309`),
  clock-bounded removal stability, and chain-replicated relay-inbox custody.
- **Privacy:** guard-closed Sphinx/AEZ v5 onion Kleisli pipelines (#834, #843, #880, #1314) over
  fixed-width cells, dormant-by-default outbound lanes (`OnionIdleFloor::OFF`) with active-window
  constant-rate slot pacing (substituting real cells for cover, plus optional background cover floor),
  paid admission, size classes, SURB reply blocks, persisted entry guards, world-facing
  `tcp`/`https`/`udp` exits, and composable `invoke<In, Out, L>` pipelines (`crates/node/src/onion`);
  the selective TUN gateway (`crates/gateway`) and onion WebView (`crates/webview`).
- **Runtimes & tooling:** native daemon and `rings` CLI, `rings-tui` interactive terminal dashboard
  (`crates/tui`), JSON-RPC + `/status?view=observability`, C FFI (Python/Swift/Kotlin bindings),
  browser provider (`@ringsnetwork/rings-node`), Chrome MV3 extension, and hosted console at
  [rings.rs](https://rings.rs/#node).
- **Protocols:** pure `Protocol` + namespace-scoped `Interpret` shells (RFC #594), with
  built-in TCP/UDP relay tunnels, echo, authoritative DNS relay (`rings-dns-relay`), and SMTP/IMAP
  mail gateway (`rings-mail-gateway`).

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
