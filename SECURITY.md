# Security And Overlay Threat Model

## Reporting a Vulnerability

Report vulnerabilities privately, through GitHub's
[private vulnerability reporting](https://github.com/RingsNetwork/rings/security/advisories/new)
or by email to [dev@rings.rs](mailto:dev@rings.rs). Do not open a public issue for a
security-sensitive report. Fixes land on `master` and ship in the next release; releases
before 1.0 are not maintained in parallel, so only the latest release is supported.

## Summary

Rings authenticates every peer and every protocol message by a DID and a delegated
signing key. Authentication proves control of a key; it does not make keys scarce. The
overlay therefore gives its guarantees to controlled and authenticated-open deployments,
and treats Sybil and eclipse resistance as a non-goal before 1.0 (see
[Non-Goals](#non-goals)).

Rings has two layers with different contracts, drawn under
[Layer Contracts](#layer-contracts): **the communication layer minimizes leakage; the
privacy layer provides privacy.**

## Assumptions

- A DID identifies an account key, and a signature check authenticates control of that
  key. Delegated keys are accepted only under the delegation checks in `rings-core`.
- An established WebRTC connection provides channel security.
- `network_id` separates overlays by configuration; it is not a secret. Every signature
  is bound to `network_id` and to a per-message-family domain tag, so a signature is not
  valid in another overlay or as another message family.
- Honest peers run the protocol, refresh descriptors before expiry, and take part in
  stabilization and storage replication.

## Fault Model

The overlay tolerates churn and fail-stop faults: peers disconnect, crash, restart, and
miss heartbeats. TTLs, stabilization, storage repair, and descriptor refreshes are
designed for that environment. Onion exits draw a fresh process epoch at each start and
bind it into their descriptors and forward layers, so cells built for a previous process
are refused after a restart.

Byzantine faults are attributable but not prevented. A malicious peer can drop, delay,
or refuse messages, ignore the service policy it advertises, or withhold stored data;
every such claim it makes is signed by its DID.

## Deployment Models

| Model | Fit | Assumption |
|---|---|---|
| Controlled membership | Supported | Operators decide which keys join, or run a private overlay of known peers. |
| Authenticated open membership | Supported with application policy | Anyone can create a DID; applications add their own allowlists, quotas, or authorization on top of the overlay's per-origin bounds. |
| Permissionless adversarial membership | Not yet supported | Needs a defence against identity-rich adversaries choosing ring positions (#780). |

## Layer Contracts

A property belongs to the communication layer only if the plain relay delivers it to
every message; everything that needs a circuit belongs to the privacy layer. Encryption
to a DID is not anonymity, and a relay leak is not something cover traffic can absorb.

### Communication layer

`crates/core`: Chord routing, `MessageRelay`, signed `MessagePayload`s, and the E2E
ElGamal stream family. The contract is payload authenticity for every message and
payload confidentiality between peers that completed the E2E handshake. There is no
unlinkability contract, and none can be added by changing the relay: Chord routes by the
destination DID, and the transaction signature names the origin.

Every hop, and the destination, learns:

- the origin DID, from the transaction signature;
- the destination DID, from which the next hop is chosen;
- its own predecessor (the authenticated edge the message arrived on) and successor;
- the encoded size and arrival time;
- the delivery stage, and nothing else about the route: the relay carrier holds only
  `next_hop`, `destination`, a hop budget, and whether the route has crossed its aim.

A request that carries a signed `reply_via` names one of the origin's links while the
origin has no predecessor (during join, and after its predecessor departs). A responder
routes at most one successor or connection answer to that peer, which hands it on only
over a direct link to the origin; every other report goes straight to the origin, so a
small request cannot be reflected as a large report onto a third party.

Confidentiality is opt-in by construction. A DID is a 160-bit digest of the account key,
so a lookup yields an address, not a key to encrypt to; a sender first completes the E2E
handshake, which carries the peer's signed account key, and then encrypts to it. Any
message outside the E2E stream family is readable by every hop and by a storage owner
holding it for an offline recipient.

The layer's obligations are leak-minimization obligations: no hop history on the wire,
no telemetry beyond what routing needs, payloads encrypted once the E2E handshake has
completed, and domain-separated signatures. Routes are loop-free: each greedy hop moves
strictly closer to its aim, and a route crosses its aim at most once, so a route takes
at most `2|V|` hops and an unreachable destination fails fast. The hop budget is outside
every signature; it bounds honest work per message and is not a promise a dishonest hop
keeps. Delegation references ([details](docs/src/advanced-topic/delegation-references.md))
replace repeated inline delegations on one link and do not change what a hop learns.

A leak on this layer is a communication-layer bug: circuits run above the relay and
inherit whatever it exposes.

### Privacy layer

`crates/node/src/onion`: layered ElGamal-AEAD circuits over direct edges, fixed-batch
cover with pacing, replay witnesses, fixed cell size classes, entry guards, and route
selection from the online-node and onion-exit registries.

- **Per-hop knowledge.** Each relay removes one layer and learns only its predecessor
  and successor on the circuit. Circuit ids are rewritten at every hop, and the
  client/exit return id travels only inside the exit layer. Only the exit sees the
  application payload; only the client knows the whole route. A route has at most eight
  hops and defaults to three, counting the exit.
- **Cover and pacing.** Every non-empty batch toward a next hop holds exactly `B = 4`
  cells, real cells padded with authenticated one-hop cover, after a pacing delay drawn
  from `[5, 25]` ms. Cells use fixed size classes from 4 KiB to 12 MiB, and a relay
  preserves the visible class across an edge. An idle link sends nothing, so links are
  batch-shaped, not constant-rate.
- **Exit replay.** Exit adapters share one process-local forward-nonce witness, so one
  `(peer, circuit, nonce)` authorizes at most one action across all installed services;
  the process epoch covers restarts.
- **Entry guards.** A client pins a small persisted set of first hops per network and
  replaces a guard only when it is no longer live, no longer eligible, or a healthier
  eligible set exists, which limits how many relays observe the client edge over time.

A circuit hides the hops from one another and the client from the exit. It does not hide
the client from its first hop, overlay membership (joining and publishing descriptors are
signed acts), the registry lookups (visible to the storage owners of those topics), the
relay signalling that sets up the first edge, or activity timing from an observer of every
link. Route anonymity also depends on the candidate set: reliability weighting may reorder
eligible candidates but never adds one, and a party with many identities can try to hold
several positions of one route.

## Feature Boundaries

### DID Identity

DID signatures authenticate the key behind a message, descriptor, or delegation. They do
not show that two DIDs belong to different operators.

### Replay And Rate Bounds

The final destination keeps a persisted 32-sequence replay window per
`(network_id, origin account, destination, traffic class)` stream, so a transaction is
dispatched at most once while the replay store is retained. Duplicates, forks, and stale
sequences are rejected as distinct verdicts; a damaged record makes only its own stream
unavailable. Exactly-once application effects are not claimed. See
[Transaction Replay Protection](docs/src/advanced-topic/transaction-replay.md).

After both signatures verify, the destination charges per-origin message and byte token
buckets keyed by the origin's account DID, so rotating a delegated key or switching relays
does not reset an allowance. Replay classification and quota admission commit together,
before the logical mailbox and application code; over-quota traffic never reaches either,
and a quota drop never disconnects the relay that carried it. A protocol that declares
delegated admission (none shipped does) skips only the per-origin message count for its
direct neighbours and still pays a byte floor per message; see
[config.yaml](docs/src/advanced-topic/config.yaml.md).

### Local Measurement And Credit

Credit and reliability are local, advisory projections of a node's own authenticated
observations. They may reorder eligible candidates; they never add a candidate, prove
membership, or change DHT ownership or placement. The ledger is bounded (16,384 peers,
least-recently-authenticated eviction), and credit stays neutral until a peer has supplied
1,000,000 useful bytes. Provisional service receipts are isolated from both and change
neither routing nor credit; see
[Provisional DRanking Service Receipts](docs/src/advanced-topic/dranking-service-receipts.md).

### Chord Routing

Chord gives deterministic, loop-free routing over the observed topology and assumes the
node set fits the deployment model. Every decoder that admits relayed bytes (wire
envelope, message body, chunk framing and reassembly, SDP, JSON-RPC body) has generated
decode-boundary tests in its owning crate.

### Connection Admission

Each node bounds its connection records, handshaking or admitted, at twice its topology
reference slots. At the bound, a new reservation evicts an admitted peer that no local
topology slot references (a revoked generation first, then the longest-silent peer past a
retention grace), or is rejected; a peer the local topology references is never evicted.
The bound limits resources; it does not decide who may join.

### Control API

A native node serves an internal and an external JSON-RPC listener behind one owner-only
Bearer token. Only the handshake methods `nodeDid` and `answerOffer` on the external
listener are public; a batch is authorized by its strictest member, and an undecodable or
unknown call never classifies as public. Both listeners require
`Content-Type: application/json` and exact configured browser origins, and the external
listener binds a non-loopback address only on explicit opt-in. The token controls the
node; it is not an admission credential.

### DHT Storage

Ownership and replication are topology-derived, and CRDT joins, owner checks, and read
repair converge among honest or fail-stop owners; a Byzantine owner can still withhold
data. Every entry's retention is capped at the maximum TTL, every carrier is bounded in
element count and element size, and versions too far ahead of the receiver's clock are
rejected. A relay inbox for an offline peer is verified element by element by its owner,
removable only by its recipient, never returned to anyone else, and bounded to its newest
64 messages. Values, held messages included, are stored in the clear; confidentiality is
the E2E layer's.

### Online And Onion Registries

Descriptors are signed and expire, which bounds stale records and makes every advertised
claim attributable.

### Native Gateway

The TUN gateway starts only under an explicit `enabled: true` or `rings run --gateway`;
the section `rings init` writes is inert until then. The generated plan captures no
destination until the operator lists a prefix, and the host capabilities a TUN device
needs are never granted by configuration. See [Native Gateway](docs/src/native-gateway.md).

## Non-Goals

- Sybil and eclipse resistance. DIDs must stay key digests for Ethereum and Solana
  addressing, so the planned defence takes position choice away instead of making
  identities costly: epoch-randomized ring positions, `pos(did, e) = H(did, beacon(e))`
  (#780).
- Unlinkability on the communication layer; it is a privacy-layer property.
- Traffic-analysis resistance against a global observer, or anonymity from routes drawn
  from a Sybil-permissive registry.
- Availability against Byzantine storage owners or route candidates.
- Stake, proof-of-work admission, or globally portable reputation.

## Known Limitations

- #908: a frame held on an unresolved delegation reference can be released past the
  replay window and lost as stale (never admitted twice).
- #912: the browser replay store writes IndexedDB under the default durability hint.
- #915: an absent replay record is indistinguishable from a new stream, which restarts
  from its first sequence.
- #916: flushed replay writes cap a node near 50 replay transitions per second on macOS.
