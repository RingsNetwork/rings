# Security And Overlay Threat Model

## Reporting a Vulnerability

Report privately through GitHub's
[private vulnerability reporting](https://github.com/RingsNetwork/rings/security/advisories/new)
or to [dev@rings.rs](mailto:dev@rings.rs), never in a public issue. Fixes ship in the next
release; before 1.0 only the latest release is supported.

## Summary

Every peer and message is authenticated by a DID and a delegated signing key. That proves
control of a key, not that keys are scarce, so Sybil and eclipse resistance is not yet
provided (see [Non-Goals](#non-goals)). The rule between the two layers is: **the
communication layer minimizes leakage; the privacy layer provides privacy.**

## Assumptions

- **Key authentication vs. identity scarcity.** A signature check authenticates control of a
  DID's key; it does not show that two DIDs belong to different operators.
- **Transport security.** An established WebRTC connection provides channel confidentiality
  and integrity between adjacent hops.
- **Domain separation.** Every signature is bound to its overlay's `network_id` and a
  per-message-family domain tag. `network_id` separates overlays; it is not a secret.
- **Honest participation.** Honest peers run the protocol, refresh descriptors, and take part
  in stabilization and replication.
- **Process memory.** A party that can read a node's process memory already holds the node's
  long-lived delegatee key. Wiping derived key material on drop is defence in depth against
  later reads, not an isolation boundary.

## Fault Model

- **Churn and fail-stop faults** are handled: peers disconnect, crash, restart, and miss
  heartbeats, while TTLs, liveness probes, stabilization, and storage repair recover state.
- **Byzantine faults** are attributable where signed, but availability against Byzantine
  routes or storage owners is not guaranteed: a peer can drop or delay traffic, ignore its
  advertised policy, or withhold stored data.

## Deployment Models

| Model | Fit |
|---|---|
| Controlled membership: operators choose which keys join | Supported |
| Authenticated open membership: anyone can create a DID | Supported, with application allowlists or authorization on top of per-slot and per-origin bounds |
| Permissionless adversarial membership | Not yet: needs ring positions that identities cannot choose (#780) |

## Layer Contracts

A property belongs to the communication layer only if the plain relay gives it to every
message; anything that needs an onion loop belongs to the privacy layer.

### Communication layer

`crates/core` provides payload authenticity for every message and end-to-end confidentiality
after the E2E handshake.

- **What hops and destinations learn:** the origin DID (from the transaction signature), the
  destination DID, their own immediate predecessor and successor, message size, arrival time,
  and whether the route has crossed its aim.
- **Confidentiality:** opt-in via the E2E handshake. Messages outside the E2E stream family
  are readable by forwarding hops and by storage owners holding them for an offline recipient.
- **Routing metadata:** routes are loop-free and carry no hop history, except for multi-hop
  connect signaling (`#947`), which records the direct-link relay path so the SDP answer or
  counter-offer can retrace it.

### Privacy layer

`crates/node/src/onion` carries client-sealed loops (`g → r → exit → r' → g`) of fixed-width
Sphinx cells over direct edges through a pinned entry guard `g`.

- **Per-hop visibility:** each relay peels one Sphinx/AEZ layer, learns only its immediate
  predecessor and successor on the loop, and keeps no per-circuit session state. Only the
  target exit opens the client's payload, and only the client knows the full route and
  recognises the reply by its return tag. Exit replies carry no exit signature and are
  deniable.
- **Exit capabilities:** an exit descriptor signs a closed capability set (`tcp` or `https`
  fetch); unknown service names are rejected and never fall back to another service.
- **Traffic shaping and admission:** links emit fixed-size-class cells at bounded active or
  idle rates, substituting queued real cells for cover traffic while link credit allows
  ([details](docs/src/advanced-topic/architecture.md)). Each hop charges sender and global
  admission budgets before decoding a cell or computing ECDH, and rejects replayed layers
  within the process epoch.
- **Stream integrity:** end-to-end sequence gaps or session failures abort the stream rather
  than presenting a truncated response as complete.
- **Entry guards:** clients persist a small set of first-hop guards on disk so restarts do not
  repeatedly expose the client edge to newly sampled relays.
- **What loops do not hide:** the client from its entry guard, overlay membership, directory
  lookups, round-trip time from the guard, cell size class and expiry quantum, or a link's
  active/idle phase and credit release rate from a local observer of that link. Route security
  also depends on candidate diversity in the relay/exit directories.

## Feature Boundaries

- **Replay and delivery.** Destinations track a bounded sequence window per
  `(network_id, origin, destination, traffic class)` stream in a crash-consistent,
  exclusively locked store so each transaction is admitted at most once while the store is
  retained ([details](docs/src/advanced-topic/transaction-replay.md)). In browsers with
  ephemeral or private storage, replay state lasts only as long as the browser retains origin
  storage.
- **Resource isolation and rate limits.** Because origin DIDs are free key pairs, bounded
  receiver tables and removal stores charge capacity to the authenticated connection slot
  (`SlotRoster` / `TableAccounting`) with a per-slot floor plus a shared pool
  (`papers/load-bounds.tex`, Law 1), paired with per-origin rate and byte buckets at the
  destination.
- **Application boundary.** Core protocol lanes never block on application callbacks.
  Application inbox delivery is bounded and lossy by design; protocols requiring reliable
  delivery enforce completeness end to end
  ([details](docs/src/advanced-topic/transport-flow-control.md#application-inbox)).
- **Transport flow control and liveness.** Data lanes are credit flow controlled so one peer's
  backpressure does not stall another peer's control lane. Liveness is judged by bounded
  challenge-response probes independent of data-lane credit
  ([details](docs/src/advanced-topic/transport-flow-control.md)).
- **Delegation references.** Links may reference previously verified delegations under
  receiver-granted duration leases bounded per link, falling back to full inline verification
  when unleased ([details](docs/src/advanced-topic/delegation-references.md)).
- **DHT storage and relay custody.** Data and relay-inbox carriers are bounded in count, size,
  TTL, and clock-skew tolerance. Relay inboxes live in a dedicated retained store, accept
  writes only from authorized routing positions, and replicate down a chain of custody-capable
  successors to survive individual node churn; nodes without persistent relay storage opt out
  of custody in their handshake
  ([data storage](docs/src/advanced-topic/chord.md#data-topic-storage),
  [relay inbox](docs/src/advanced-topic/chord.md#relay-inbox)).
- **Local APIs and gateways.**
  - **Control API:** guarded by an owner Bearer token except for public WebRTC handshake
    methods (`nodeDid`, `answerOffer`); non-loopback binding and CORS origins are opt-in.
  - **Native TUN gateway:** disabled by default; captures only operator-configured prefixes
    and never elevates host privileges.
  - **DNS relay:** serves only records signed by the target DID owner, routes ingress traffic
    only to owner-announced `dns-relay/<port>` services, keys its IPv6 address map with a
    relay-local secret, and bounds concurrent lookups and owners. `proxy_protocol: v2` headers
    are unauthenticated across the overlay and must not be used for authorization
    ([details](docs/src/dns-relay.md)).

## Non-Goals

- **Sybil and eclipse resistance.** DIDs remain public-key digests for Ethereum and Solana
  addressing; defending ring placement against chosen identities requires epoch-randomized
  positions `pos(did, e) = H(did, beacon(e))` (#780).
- **Unlinkability on the communication layer.**
- **Resistance to a global passive adversary.**
- **Availability against Byzantine storage owners or route hops.**

## Known Limitations

- **Slot-rotation residual (#907, #1094):** an adversary that rotates fresh connections
  through free connection slots can leave those slots charged up to their floor for the
  lifetime of the admitted records. A newly admitted honest peer takes the cleanest free slot
  and retains its share of the common pool, while existing connections keep their slots.
- **Unauthenticated `EntryKind::Data` topics (#921, #1179):** shared data topics do not yet
  enforce per-signer write authority or per-origin element/tombstone quotas at the storage
  owner; readers verify descriptor signatures and content hashes locally, but a peer can fill
  or overwrite a data topic's carrier capacity until entries expire.
- **Ephemeral browser storage (#1036, #1057):** browser tabs in private/ephemeral contexts
  lose IndexedDB replay and custody state when the session closes, and tabs that close without
  a graceful leave are detected after the liveness probe timeout.
- **Cryptographic stack residue (#852, #1122):** HKDF/HMAC wrappers zeroize their key states
  on drop, while transient stack words inside the underlying SHA-256 / HKDF block schedule are
  overwritten by subsequent calls rather than explicitly zeroized.
