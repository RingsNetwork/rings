# Security Architecture and Threat Model

## Reporting a Vulnerability

Report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/RingsNetwork/rings/security/advisories/new)
or to [dev@rings.rs](mailto:dev@rings.rs). Fixes ship in the next release, and pre-1.0
support targets the latest release.

## Summary

Rings secures peer-to-peer communication and overlay services through cryptographic identity,
authenticated Chord routing, slot-isolated resource accounting, and Sphinx onion circuits.
Every peer and message is authenticated by a DID and a delegated session key, and the
architecture divides responsibilities cleanly across two layers: **the communication layer
provides authenticated routing and end-to-end encryption; the privacy layer provides
circuit-level anonymity.**

## Assumptions

- **Cryptographic identity and delegation.** Every peer authenticates control of its DID key
  pair through a short-lived delegated session key. Signatures are domain-separated by
  message family and bound to the overlay's `network_id`.
- **Link-layer security.** Established WebRTC connections provide hop-by-hop confidentiality
  and integrity over DTLS/SCTP.
- **Key hygiene.** Long-lived and derived cryptographic material (including HKDF/HMAC states)
  is zeroized on drop as defence in depth.

## Fault Model

- **Churn and fail-stop resilience.** Peers can join, disconnect, or restart dynamically.
  Active challenge-response liveness probes, Chord stabilization, CRDT storage repair, and
  chain-replicated relay custody automatically heal routing and data placement.
- **Accountability and resource isolation.** Every protocol claim, transaction, and directory
  descriptor is cryptographically signed. Per-slot resource accounting, credit-based flow
  control, and multi-node replication isolate honest peers and preserve service continuity
  across untrusted hops.

## Deployment Models

| Model | Posture |
|---|---|
| Controlled membership | Full support: operators authorize participating keys |
| Open authenticated membership | Full support: combines slot-based admission, per-origin rate quotas, age-gated custody, and application-level authorization |
| Adversarial position-grinding resistance | Partial mitigation today: `PeerRing` positions are still deterministically derived from delegator public keys, so an attacker can generate keys to target a ring interval or attempt initial eclipse positioning; slot isolation, peer quality ranking, and pinned entry guards bound the impact of positioned Sybils until epoch-randomized ring placement (`pos(did, e) = H(did, beacon(e))`, #780) and DRanking trust algebra (#748) land in 2.0 |

## Layer Contracts

Base overlay guarantees apply to every message in the communication layer, while circuit
anonymity is provided by the onion privacy layer.

### Communication layer

`crates/core` provides end-to-end origin authentication for every transaction, loop-free
Chord routing, and authenticated encryption for E2E streams.

- **Minimal routing metadata:** forwarding hops see only the fields required to route—the
  signed origin DID, destination DID, payload size, and whether the route has crossed its
  target. Routes remain history-free, using a direct-link return path only during initial
  WebRTC connect signaling (#947) so the SDP answer can retrace the signaling hops.
- **End-to-end confidentiality:** applications establish E2E ElGamal encrypted streams after
  verifying the remote peer's DID key, protecting payloads across forwarding hops and storage
  nodes.

### Privacy layer

`crates/node/src/onion` provides anonymous transport over client-sealed Sphinx/AEZ loops
(`g → r → exit → r' → g`) anchored at the client's pinned entry guard `g`.

- **Hop-by-hop unlinkability:** each relay peels a single Sphinx/AEZ layer, learns only its
  immediate predecessor and successor on the loop, and retains no per-session state. Only the
  selected exit opens the payload, and only the client knows the complete route and matches
  the return tag. Exit replies carry no exit signature and remain deniable.
- **Explicit exit capabilities:** exit descriptors sign a closed capability set (`tcp` stream
  or `https` fetch) so clients and exits negotiate exact service semantics.
- **Traffic shaping and paid admission:** links emit fixed-size-class cells at bounded active
  or idle rates, substituting queued real cells for cover traffic under link credit
  ([details](docs/src/advanced-topic/architecture.md)). Relays charge per-window admission
  budgets before performing public-key operations and enforce single-use replay tags within
  each process epoch.
- **Stream Completeness:** end-to-end sequence verification ensures that applications receive
  complete, ordered streams.
- **Pinned entry guards:** clients persist their first-hop guards across restarts to maintain
  stable entry edges over time.

## Feature Boundaries

- **Replay protection and crash consistency.** Destinations maintain a persisted, exclusively
  locked sliding window per `(network_id, origin, destination, traffic class)` stream so
  admitted transactions are dispatched at most once
  ([details](docs/src/advanced-topic/transaction-replay.md)).
- **Slot-isolated resource accounting.** Bounded receiver tables and removal stores allocate
  capacity by authenticated connection slot (`SlotRoster` / `TableAccounting`) with a
  guaranteed per-slot floor plus a shared pool (`papers/load-bounds.tex`, Law 1), paired with
  per-origin message and byte token buckets at the destination.
- **Non-blocking application boundary.** Core protocol lanes run independently of application
  callbacks. Application delivery uses bounded queues, and stream protocols enforce end-to-end
  reliability
  ([details](docs/src/advanced-topic/transport-flow-control.md#application-inbox)).
- **Credit flow control and independent liveness.** Data lanes are credit flow controlled so
  load on one peer never stalls another peer's control lane. Peer liveness is verified by
  dedicated challenge-response probes
  ([details](docs/src/advanced-topic/transport-flow-control.md)).
- **Verified delegation leases.** Links cache verified session delegations under bounded,
  receiver-granted duration leases and automatically fall back to inline verification when
  unleased ([details](docs/src/advanced-topic/delegation-references.md)).
- **Durable DHT storage and replicated relay custody.** Storage carriers enforce bounded
  sizes, counts, TTLs, and clock-skew tolerances. Offline relay inboxes reside in a dedicated
  retained store, validate routing authority on write, and replicate across a chain of
  age-gated custody nodes to preserve messages across peer churn
  ([data storage](docs/src/advanced-topic/chord.md#data-topic-storage),
  [relay inbox](docs/src/advanced-topic/chord.md#relay-inbox)).
- **Local control, gateway, and DNS relay boundaries.**
  - **Control API:** protected by an owner Bearer token while exposing the public WebRTC
    handshake methods (`nodeDid`, `answerOffer`) for seamless peer onboarding; remote binding
    and CORS origins are explicit operator opt-ins.
  - **Native TUN gateway:** opt-in transparent TCP routing over onion circuits for
    operator-configured destination prefixes.
  - **DNS relay:** serves owner-signed descriptors, forwards traffic to owner-registered
    `dns-relay/<port>` services, keys IPv6 mapping with a relay-local secret, and bounds
    concurrent lookups and owners ([details](docs/src/dns-relay.md)).

<a id="non-goals"></a>

## Architectural Scope

- **Layered privacy separation:** metadata unlinkability is provided by the Sphinx onion
  privacy layer, keeping the base Chord communication layer lightweight and direct.
- **Permissionless wallet-compatible DIDs:** DIDs remain standard public-key digests
  compatible with Ethereum, Solana, and WebCrypto keys; ring-position randomization (#780)
  and DRanking trust algebra (#748) provide topological hardening without requiring
  proof-of-work identity minting.

<a id="known-limitations"></a>

## Operational Notes

- **Browser storage lifecycle (#1036, #1057):** persistent contexts commit replay and custody
  state to IndexedDB under exclusive Web Locks, while private/ephemeral browsing contexts
  scope state to the active browser session.
- **Cleanest-first slot allocation under churn (#907, #1094):** newly admitted connections
  are assigned the cleanest available slot in `SlotRoster`, preserving access to the shared
  capacity pool even during high connection turnover.
- **Zeroized cryptographic wrappers (#852, #1122):** HKDF, HMAC, and ElGamal key-derivation
  wrappers zeroize their key states on drop, while transient register words inside the inner
  SHA-256 block schedule are overwritten by subsequent invocations.
