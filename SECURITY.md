# Security Architecture and Threat Model

## Reporting a Vulnerability

Report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/RingsNetwork/rings/security/advisories/new)
or to [dev@rings.rs](mailto:dev@rings.rs), never in a public issue. Fixes ship in the next
release; before 1.0 only the latest release is supported.

## Summary

Rings secures peer-to-peer communication and overlay services through cryptographic identity,
authenticated routing, slot-isolated resource accounting, and Sphinx onion circuits. Every
peer and message is authenticated by a DID and a delegated session key, and the architecture
separates transport concerns into two complementary layers: **the communication layer
minimizes routing metadata; the privacy layer provides circuit anonymity.**

## Assumptions

- **Cryptographic identity and delegation.** Every peer authenticates control of its DID key
  pair through a short-lived delegated session key. Signatures are domain-separated by
  message family and bound to the overlay's `network_id`.
- **Link-layer security.** Established WebRTC connections provide hop-by-hop confidentiality
  and integrity over DTLS/SCTP.
- **Key hygiene.** Long-lived and derived cryptographic material (including HKDF/HMAC states)
  is zeroized on drop as defence in depth.

## Fault Model

- **Churn and fail-stop resilience.** Peers may disconnect, restart, or miss heartbeats at
  any time. Active challenge-response liveness probes, Chord stabilization, CRDT storage
  repair, and multi-replica relay custody automatically heal routing and data placement.
- **Accountability and bounded impact under adversarial peers.** Every protocol claim,
  transaction, and directory descriptor is cryptographically signed. While an individual relay
  or storage peer can drop or delay traffic passing through it, per-slot resource accounting,
  credit flow control, and multi-node replication bound the impact of misbehaving peers.

## Deployment Models

| Model | Posture |
|---|---|
| Controlled membership | Full support: operators authorize participating keys |
| Open authenticated membership | Full support: combines slot-based admission, per-origin rate quotas, age-gated custody, and application-level authorization |
| Adversarial position-grinding resistance | Hardened today by slot isolation, peer quality ranking, and pinned entry guards; epoch-randomized ring placement (`pos(did, e) = H(did, beacon(e))`, #780) is planned for 2.0 |

## Layer Contracts

A property belongs to the communication layer when the base overlay provides it to every
message; circuit-level anonymity belongs to the privacy layer.

### Communication layer

`crates/core` provides end-to-end origin authentication for every transaction, loop-free
Chord routing, and authenticated encryption for E2E streams.

- **Minimal routing metadata:** forwarding hops see only the fields required to route—the
  signed origin DID, destination DID, payload size, and whether the route has crossed its
  target. Routes carry no hop history except during initial WebRTC connect signaling (#947),
  where the SDP answer retraces the direct-link signaling path.
- **End-to-end confidentiality:** applications establish E2E encrypted streams after verifying
  the remote peer's DID key, protecting payloads from forwarding hops and intermediate storage
  nodes.

### Privacy layer

`crates/node/src/onion` provides anonymous transport over client-sealed Sphinx/AEZ loops
(`g → r → exit → r' → g`) anchored at the client's pinned entry guard `g`.

- **Hop-by-hop unlinkability:** each relay peels a single Sphinx/AEZ layer, learns only its
  immediate predecessor and successor on the loop, and retains no per-session state. Only the
  selected exit opens the payload, and only the client knows the complete route and matches
  the return tag. Exit replies carry no exit signature and remain deniable.
- **Explicit exit capabilities:** exit descriptors sign a closed capability set (`tcp` stream
  or `https` fetch); unadvertised services are rejected without fallback.
- **Traffic shaping and paid admission:** links emit fixed-size-class cells at bounded active
  or idle rates, substituting queued real cells for cover traffic under link credit
  ([details](docs/src/advanced-topic/architecture.md)). Relays charge per-window admission
  budgets before performing public-key operations and reject replayed cells within the process
  epoch.
- **Stream integrity:** end-to-end sequence verification aborts incomplete streams cleanly so
  truncated data is never presented to the application as complete.
- **Pinned entry guards:** clients persist their first-hop guards across restarts to defend
  against long-term guard-rotation exposure.

## Feature Boundaries

- **Replay protection and crash consistency.** Destinations maintain a persisted, exclusively
  locked sliding window per `(network_id, origin, destination, traffic class)` stream so
  admitted transactions are dispatched at most once while storage is retained
  ([details](docs/src/advanced-topic/transaction-replay.md)).
- **Slot-isolated resource accounting.** Bounded receiver tables and removal stores charge
  capacity to the authenticated connection slot (`SlotRoster` / `TableAccounting`) with a
  guaranteed per-slot floor plus a shared pool (`papers/load-bounds.tex`, Law 1), complemented
  by per-origin message and byte token buckets at the destination.
- **Non-blocking application boundary.** Core protocol lanes never block on application
  handlers. Application delivery uses bounded queues, and reliable stream protocols enforce
  completeness end to end
  ([details](docs/src/advanced-topic/transport-flow-control.md#application-inbox)).
- **Credit flow control and independent liveness.** Data lanes are credit flow controlled so
  backpressure on one peer never stalls another peer's control lane. Peer liveness is judged
  by dedicated challenge-response probes
  ([details](docs/src/advanced-topic/transport-flow-control.md)).
- **Verified delegation leases.** Links cache verified session delegations under bounded,
  receiver-granted duration leases and fall back to inline signature verification whenever a
  lease is absent ([details](docs/src/advanced-topic/delegation-references.md)).
- **Durable DHT storage and replicated relay custody.** Storage carriers enforce bounded
  sizes, counts, TTLs, and clock-skew tolerances. Offline relay inboxes reside in a dedicated
  retained store, validate routing authority on write, and replicate across a chain of
  age-gated custody nodes so messages survive individual peer churn
  ([data storage](docs/src/advanced-topic/chord.md#data-topic-storage),
  [relay inbox](docs/src/advanced-topic/chord.md#relay-inbox)).
- **Local control, gateway, and DNS relay boundaries.**
  - **Control API:** protected by an owner Bearer token except for the public WebRTC handshake
    methods (`nodeDid`, `answerOffer`); non-loopback listeners and CORS origins require
    explicit opt-in.
  - **Native TUN gateway:** opt-in only; captures only operator-configured prefixes without
    elevating host privileges.
  - **DNS relay:** serves only owner-signed descriptors, forwards traffic only to
    owner-registered `dns-relay/<port>` services, keys IPv6 mapping with a relay-local secret,
    and bounds concurrent lookups and owners ([details](docs/src/dns-relay.md)).

## Non-Goals

- **Unlinkability on the base communication layer** (provided instead by the onion privacy
  layer).
- **Resistance to a global passive traffic observer.**
- **Cryptographic identity pricing:** DIDs remain standard public-key digests compatible with
  Ethereum and Solana addressing; position-grinding defence is addressed via epoch-randomized
  ring placement (#780) and trust algebra (#748) rather than proof-of-work identity minting.

## Known Limitations

- **Browser ephemeral storage contexts (#1036, #1057):** in private browsing or unpersisted
  IndexedDB contexts, replay windows and held custody state last for the browser session
  lifetime, and tabs closed without a lifecycle event are detected on the next liveness probe
  cycle.
- **Connection-churn slot residual (#907, #1094):** rapid connection rotation can temporarily
  consume the per-slot floor of free slots until admitted entries expire; newly admitted peers
  are assigned the cleanest available slot and retain access to the shared pool.
- **Underlying hash-schedule stack words (#852, #1122):** HKDF/HMAC wrappers zeroize their key
  structs on drop, while transient register/stack words inside the inner SHA-256 block
  schedule are overwritten by subsequent calls.
