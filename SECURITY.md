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

- A signature check authenticates control of a DID's key. It does not show that two DIDs
  belong to different operators.
- An established WebRTC connection provides channel security.
- Every signature is bound to its overlay's `network_id` and a per-message-family domain
  tag. `network_id` separates overlays; it is not a secret.
- Honest peers run the protocol, refresh descriptors, and take part in stabilization and
  replication.

## Fault Model

Churn and fail-stop faults are handled: peers disconnect, crash, restart, and miss
heartbeats, and TTLs, stabilization, storage repair, and descriptor refreshes recover.
Byzantine faults are attributable but not prevented: a peer can drop or delay messages,
ignore its advertised policy, or withhold data, and every claim it makes is signed.

## Deployment Models

| Model | Fit |
|---|---|
| Controlled membership: operators choose which keys join | Supported |
| Authenticated open membership: anyone can create a DID | Supported, with application allowlists or authorization on top of the per-origin bounds |
| Permissionless adversarial membership | Not yet: needs ring positions that identities cannot choose (#780) |

## Layer Contracts

A property belongs to the communication layer only if the plain relay gives it to every
message; anything that needs a circuit belongs to the privacy layer.

### Communication layer

`crates/core` provides payload authenticity for every message, and confidentiality after
the E2E handshake. Every hop, and the destination, learns:

- the origin DID, from the transaction signature;
- the destination DID, from which the next hop is chosen;
- its own predecessor and successor;
- the message's size and arrival time;
- whether the route has crossed its aim, and nothing else about the route.

Unlinkability cannot be added here: Chord routes by destination, and the signature names
the origin. Confidentiality is opt-in: a DID is a key digest, so a sender first runs the
E2E handshake to learn the peer's key. Messages outside the E2E stream family are
readable by every hop and by a storage owner holding them for an offline recipient.

Routes are loop-free and take at most `2|V|` hops. While an origin has no predecessor, its
requests may name one of its links in a signed `reply_via`, which receives at most one
successor or connection answer and forwards it only over a direct link. The hop budget is
unsigned: it bounds honest work, not a dishonest hop. A leak here is a communication-layer
bug; circuits inherit whatever the relay exposes.

### Privacy layer

`crates/node/src/onion` builds layered ElGamal-AEAD circuits over direct edges.

- **Per-hop knowledge.** Each relay learns only its predecessor and successor; circuit ids
  are rewritten at every hop. Only the exit sees the payload; only the client knows the
  route. Routes have at most eight hops, three by default.
- **Cover and pacing.** Every non-empty batch toward a next hop holds exactly 4 cells,
  padded with authenticated cover, after a 5–25 ms delay. Cells use fixed size classes
  from 4 KiB to 12 MiB. Idle links send nothing.
- **Replay.** One `(peer, circuit, nonce)` authorizes at most one exit action, and each exit
  draws a fresh process epoch per start, which invalidates cells built before a restart.
- **Entry guards.** A client pins a persisted set of first hops, limiting how many relays
  see its edge over time.

A circuit hides hops from each other and the client from the exit. It does not hide the
client from its first hop, overlay membership, registry lookups, or timing from an observer
of every link. Anonymity also depends on the candidate set: registry descriptors are signed
and expire, but a party with many identities can try to hold several positions of a route.

## Feature Boundaries

- **Replay.** The destination keeps a persisted 32-sequence window per
  `(network_id, origin, destination, traffic class)` stream, so a transaction is dispatched
  at most once while the store is retained; a damaged record disables only its stream.
  Exactly-once effects are not claimed.
  [Details](docs/src/advanced-topic/transaction-replay.md).
- **Rate limits.** After verification, the destination charges per-origin message and byte
  buckets keyed by the account DID, committed together with replay and before application
  code. Rotating keys or relays does not reset them, and a drop never disconnects the relay.
- **Measurement and credit.** Local and advisory: they may reorder eligible candidates but
  never add one or change DHT placement. Service receipts affect neither.
- **Connection admission.** Connection records are bounded at twice the topology's
  reference slots; at the bound, only peers no topology slot references can be evicted.
- **Control API.** One owner-only Bearer token guards both JSON-RPC listeners; only the
  handshake methods `nodeDid` and `answerOffer` are public. The external listener binds a
  non-loopback address only on opt-in, and browser requests must come from configured origins.
- **DHT storage.** Retention is capped at the maximum TTL; carriers are bounded in count and
  size; versions too far ahead of the receiver's clock are rejected. Each data-topic element
  expires at its own horizon, and a removal is collected only once every write it covers has
  expired everywhere within the clock-skew tolerance, so a removed value is not resurrected
  while the carrier is retained. A relay inbox is verified by its owner, readable and
  removable by its recipient, and capped at 64 messages; its holder removes only an element
  that fails the witness for good, which no write could have admitted. Values are stored in
  the clear.
- **Transport flow control.** Every lane of a connection is credit flow controlled: a receiver
  holds at most 16 frames per lane, refuses and reports a frame beyond its advertised credit,
  and refuses no honest frame for want of credit. Overload is pushed back to the sender and
  every wait is logged. Above 16 MiB of received frames a node defers new credit on every
  lane but the control lane, a soft bound; the hard bound is per connection (4 MiB), so a
  node's worst case grows with its admitted connections (#934). A peer that withholds the
  control lane's credit is evicted by liveness within its idle interval plus the answer
  window, whether or not it keeps sending, and one peer's backpressure never holds another's
  link: every send a handler makes (a forward, a report, a query) returns once queued.
  [Details](docs/src/advanced-topic/transport-flow-control.md).
- **Wire decoding.** Every decoder that admits relayed bytes has generated malformed-input
  tests in its crate.
- **Native gateway.** A TUN gateway starts only on `enabled: true` or `--gateway`, captures
  nothing until the operator lists a prefix, and never grants itself host capabilities.

## Non-Goals

- Sybil and eclipse resistance. DIDs stay key digests for Ethereum and Solana addressing, so
  the planned defence removes position choice instead of pricing identities:
  `pos(did, e) = H(did, beacon(e))` (#780).
- Unlinkability on the communication layer.
- Resistance to a global traffic observer.
- Availability against Byzantine storage owners or route candidates.

## Known Limitations

- #908: a frame held on an unresolved delegation reference can be lost as stale, never
  admitted twice.
- #912: the browser replay store uses IndexedDB's default durability hint.
- #915: a missing replay record restarts its stream from the first sequence.
- #916: durable replay writes cap a node near 50 transitions per second on macOS.
- #918: a registry holds about 25 KB of tombstones per registrant at the default heartbeat.
- #920: storage byte-budget eviction can drop a removal before it is collected, resurrecting
  the values it covered.
- #921: data-topic tombstones are bounded by rate, not by count or bytes, and a forged removal
  received by sync can hold a topic live for up to the horizon past the receiver's clock.
- #932: a reassembled message whose full-size mailbox charge does not fit is dropped.
