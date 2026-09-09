# Security And Overlay Threat Model

This document describes the current security boundary for the Rings overlay. It is
documentation, not a claim that every stronger model is implemented. Rings has two
layers with different contracts, drawn under [Layer Contracts](#layer-contracts):
the communication layer minimizes leakage; the privacy layer provides privacy.

## Summary

Rings authenticates peer identities and protocol messages with DIDs and delegated
session keys. That proves control of a key. It does not make identities scarce,
expensive, or globally reputation-bearing. Peer measurements remain local advisory
state and are not portable trust claims.

The current Chord overlay must therefore not be treated as Sybil-resistant
permissionless membership. A party that can create many identities can influence
successor, predecessor, finger-table, storage-owner, and onion-route candidate
placement, and can attempt eclipse behavior.

## Assumptions

- A DID identifies a cryptographic account key, and signature checks authenticate
  control of that key.
- Delegated session keys are accepted only according to the protocol checks in the
  core and node layers.
- WebRTC transport establishment and data channels provide the expected channel
  security for a successfully established peer connection.
- `network_id` separates overlays by configuration. It is not an access-control
  secret, because a peer can choose to use the same value. Every message and
  descriptor signature is nevertheless bound to the signer's `network_id` and to a
  per-message-family domain tag, so a signature issued inside one overlay does not
  verify inside another, and a signature over one message family does not verify
  as a different family that shares the same signing surface.
- Honest peers run the same protocol rules, refresh descriptors before expiry, and
  participate in stabilization and storage replication.

## Fault Model

The implementation is intended to handle ordinary churn and fail-stop behavior:
peers can disconnect, crash, restart with a new session, or miss heartbeats. TTLs,
stabilization, storage repair, and descriptor refreshes are designed for that
environment.

The current overlay does not provide Byzantine membership safety. A malicious peer
can drop, delay, or refuse messages; advertise service policy it later ignores;
withhold stored data; or create many identities to bias topology position. Signed
descriptors make these claims attributable to a DID, but they do not make the DID
costly to create.

## Deployment Models

| Model | Current fit | Required assumption |
|---|---|---|
| Controlled membership | Supported | Operators decide which keys may join, or run a private overlay whose peers are known. |
| Authenticated open membership | Partially supported | Anyone can create a DID, but applications accept the Sybil risk and add their own policy, quotas, or allowlists. |
| Permissionless adversarial membership | Not currently supported | The network would need an admission cost or other Sybil/eclipse mitigation before making strong availability, routing, or privacy claims. |

## Current Non-Goals

- Sybil resistance from DID authentication alone.
- Eclipse resistance against an attacker that can choose many DIDs.
- Strong public-network availability when storage owners or route candidates are
  adversarial.
- Sender or receiver unlinkability on the communication layer. Chord routes by the
  destination DID and every payload names its origin by signature, so the plain
  relay cannot hide either endpoint; this is a boundary of the layer, not missing
  work on the relay. Unlinkability is a privacy-layer property.
- Strong anonymity or traffic-analysis resistance for privacy-layer routes chosen
  from a Sybil-permissive live-node registry.
- Economic security, stake weighting, proof-of-work admission, globally trusted or
  portable reputation, or globally rate-limited identity issuance.

## Layer Contracts

Rings has two layers with different security contracts, and the boundary between
them is where every privacy claim is decided. The rule is: **the communication
layer minimizes leakage; the privacy layer provides privacy.** A property belongs
to the communication layer only if the plain relay delivers it to every message;
everything that needs a circuit belongs to the privacy layer. Confusing the two
produces two recurring errors: privacy properties get attributed to the plain relay,
as if encryption to a DID were anonymity, and communication-layer leaks get treated
as privacy-layer bugs that cover traffic is expected to absorb.

### Communication layer

The communication layer is `crates/core`: Chord routing, `MessageRelay`,
`MessagePayload`, session-key signatures, and the E2E ElGamal stream family. Its
contract is payload authenticity for every message and payload confidentiality for
peers that have completed the E2E handshake. It has no unlinkability contract and
cannot acquire one by changing the relay: Chord routes by the destination DID, so
every hop must see it, and every payload names its origin through the transaction
signature, so every hop can attribute it.

Every hop, and the destination, learns from a relayed message:

- the origin DID, named by the transaction signature;
- the destination DID, carried by the transaction and by the relay header, because
  the next hop is chosen from it;
- its own predecessor, the authenticated transport edge the message arrived on, and
  its successor, the relay's `next_hop`;
- the encoded size and the arrival time of the message;
- until #736 lands, the complete hop history: the relay's `path` is a push-only stack
  that every forwarding hop appends itself to, so an intermediate hop sees every node
  that handled the message before it, and the destination receives the whole route.
  After #736 the relay carries only `next_hop`, `destination`, and a hop budget, so a
  hop learns exactly its predecessor and successor and the destination learns only
  the last hop.

Confidentiality on this layer is opt-in by construction, not by policy. A DID is the
160-bit keccak digest of the account public key, so a Chord lookup by DID yields a
routable identifier and not a key to encrypt to. A sender cannot encrypt to a peer it
has only looked up; it first completes the E2E handshake, which carries the peer's
account public key under that peer's DID signature, and then sends ElGamal stream
frames encrypted to that key. A message sent outside the E2E stream family is
readable by every hop, and by the storage owner that holds it for an offline
recipient. Encrypting at the DHT itself would need join and lookup to carry keys
rather than key digests, which is a different identifier design, not a relay change.

The obligations of this layer are leak-minimization obligations:

- no hop history on the wire (#736);
- no telemetry in the envelope beyond what routing needs: the next hop, the
  destination, and the hop budget;
- payload bytes encrypted to the destination's account key once the E2E handshake
  has completed, with the signed envelope supplying the integrity that the
  malleable ElGamal frames lack on their own;
- every signature bound to `network_id` and to a per-message-family domain tag, so
  an observation in one overlay is not a credential in another.

A leak on this layer is a communication-layer bug. Cover traffic and circuits do not
fix it: they run above the relay and inherit whatever it exposes.

### Privacy layer

The privacy layer is `crates/node/src/onion`: layered ElGamal-AEAD circuits over
direct edges, fixed-batch cover cells with pacing, replay witnesses, fixed size
classes for cells, and route selection from the online-node and onion-exit
registries. It sits in `rings-node` deliberately: Chord remains the storage and
discovery substrate, and exit policy is an application decision.

**Per-hop knowledge bound.** Forward layers are wrapped from exit to entry with the
selected hops' session public keys. Each relay decrypts exactly one ElGamal-AEAD
layer and learns only the immediate next hop plus an opaque inner layer; backward
frames carry a client-encrypted AEAD payload that relays forward with local return
state. A circuit id identifies exactly one directed edge of one route and is
rewritten at every hop, and the client/exit return id is encrypted inside the exit
layer and never appears as an edge header. A relay therefore knows its predecessor
and its successor on the circuit and nothing else about the route; only the exit sees
the application payload, and only the client knows the whole route. A route is at
most eight hops and defaults to three, counting the exit.

**Cover and pacing contract** (`circuit/send_outbox.rs`). Let `B = 4` be the link
batch size. A non-empty batch toward one next hop carries `r` real cells,
`1 <= r <= B`, followed by exactly `B - r` authenticated one-hop cover cells, so
every observable batch holds `B` cells and the visible cell-count amplification is at
most `B`. One pacing delay drawn from the closed interval `[5, 25]` ms precedes each
batch. Cover is generated only for a real-driven batch and an idle lane emits
nothing, so a link is batch-shaped rather than constant-rate: an observer that sees
the link still sees when a client is active. Cells are encoded in fixed size classes,
from 4 KiB to 12 MiB, and a relay preserves the visible class across an edge so that
a shrinking cell cannot reveal route position. The bandwidth and latency these rules
cost are intentional privacy properties, not queue inefficiencies to optimize away.

**What circuits hide, and what they do not.** A circuit hides the route's hops from
one another and hides the client from the exit. It does not hide the client from its
first hop, which authenticates the client's DID on the transport edge the first cell
arrives on; it does not hide overlay membership, which is public; and it does not
hide activity timing from an observer that watches every link.

**Inherited from the communication layer, and not repairable here:**

- overlay membership is public: joining the ring and publishing a presence
  descriptor are signed, attributable actions;
- the edge to the first hop is established through Chord relay signalling, so the
  nodes that relay that handshake learn that the client and the first hop are
  connecting;
- reading the online-node and onion-exit registries reveals interest to the storage
  owners that hold those topics;
- the first hop learns the client DID.

**Candidate set.** Route security depends on the candidate set as much as on the
circuit protocol. In an authenticated-open overlay, a Sybil operator can try to
appear in several positions of one route unless the deployment adds independent
admission or diversity controls. Reliability weighting may reorder eligible
candidates; it never adds one.

## Feature Boundaries

### DID Identity

DID signatures authenticate the key behind a message, descriptor, or session
delegation. They do not prove that two DIDs are controlled by different operators,
and they do not prevent an operator from generating many DIDs.

### Local Measurement And Credit

Credit and reliability are computed independently by each node from its own
authenticated transport observations. They are advisory rather than authorization:
reliability may reorder eligible connection candidates or weight eligible onion-route
candidates, and credit is exposed for local policy, but neither value can add a peer
to the candidate set, prove Chord membership or routing correctness, determine DHT
ownership, or change storage placement.

The measurement ledger is bounded and uses least-recently-authenticated eviction.
Unknown or merely locally addressed identities cannot establish records. A residual
Sybil risk remains because authenticated DIDs are not scarce: completing authenticated
peer connections with 16,384 fresh DIDs can replace every record in a default full
ledger. The credit multiplier remains neutral until a peer has supplied 1,000,000
useful bytes, which raises the cost of earning positive credit but does not make
identities scarce or protect ledger residency. Deployments that need stronger
retention guarantees must add admission, identity-cost, or operator policy outside
the measurement subsystem.

### Chord Routing

Chord routing assumes the node set is acceptable under the deployment model. It
gives deterministic routing over the observed topology; it does not defend, by
itself, against an adversary that can occupy many positions on the identifier ring.

### Connection Admission

Each node bounds the number of peers holding any logical connection record,
whether handshaking or admitted, at twice its topology reference slots
(one slot per ring bit for fingers, the successor-list capacity, and the
predecessor). One share covers the peers this node references; the other covers
peers that reference this node, which it cannot observe because references are
directed while connections are shared; Chord places no bound on how many
peers may hold this node as a finger, so the second share is a heuristic, not
a bound. When the bound is reached, a new reservation evicts one admitted peer
that no local topology slot references: a generation already revoked by a send
failure first, otherwise the peer that has been silent longest among those
older than the retention grace. If every unreferenced peer is younger than the
grace, the reservation is rejected. The reference check and the retirement
share one critical section, so a peer referenced by the local topology at
retirement time is never evicted. Eviction happens only under admission
pressure, so an identity-rich adversary that fills the table loses one
connection per admission that honest peers attempt. The bound limits resource
use; it is not a Sybil defence.

### Control API

Each native node serves two JSON-RPC listeners guarded by one owner-only Bearer token. The
internal listener is the operator's control surface and demands the token on every route.
The external listener is the surface peers dial for the HTTP handshake: `nodeDid` and
`answerOffer` are served without the token, because the offer they exchange is already
bound to the caller's DID by its signature and the resulting connection is subject to the
admission bound above; `nodeInfo`, `lookupOnlineNodes`, `lookupOnionExits`, and `/status`
demand the token. A batch is authorized by its strictest member, so a public method
cannot carry a gated one past the check. Both listeners accept only
`Content-Type: application/json` and only exact configured browser origins, and the
external listener binds a non-loopback address only under an explicit opt-in. The token
is therefore a control credential, not an admission credential: a seed admits arbitrary
peers without sharing it, and the overlay's admission story is the one described in the
sections above.

### DHT Storage

Storage ownership and replication are topology-derived. CRDT joins, owner checks,
retention cleanup, and read repair improve convergence among honest or fail-stop
peers. They do not force a Byzantine storage owner to serve data or preserve data
it has chosen to withhold.

Every accepted entry carries a retention bound stamped by its origin and capped at
admission by the maximum time-to-live, so a peer cannot ask a storage owner to hold
a value indefinitely; expired values are retired on their next read. Each carrier is
bounded in payload count, and every payload element in encoded bytes, so one carrier
holds at most their product; when the count cap binds, the oldest payloads are the
ones dropped. Admission also rejects CRDT versions whose logical time runs ahead of
the receiver's clock by more than the message skew tolerance, so a forged version can
dominate honest writes only for that tolerance and cannot pin a key beyond it.
A relay inbox, the messages held for an offline peer, is retained longer than a data
topic; that policy is safe because the storage owner verifies every inbox element
itself: a `CustomMessage` addressed to the inbox's peer, wrapped and signed by the node
that held it, verified inside the local overlay as of the hold instant, and admitted
only from the node the owner itself routes that peer to. A removal is accepted only from
the recipient, a relocation only as an ownership hand-off from the predecessor, and a
relay carrier is never fetched, cached, replicated, or returned to a lookup by anyone
but its recipient. A relay carrier has one placement and its own storage namespace, so
a data topic any node parks at the inbox position cannot shadow the inbox. A malicious holder is one identity in one ring position: it can hold
junk for the peers it is responsible for (bounded to the newest 64 messages per inbox)
or redeliver a message inside the sender's own proof lifetime, and every element names
it by signature. Held messages are stored and relocated in the clear between owners, as
every DHT value is; confidentiality is the application's E2E layer's.
Native storage enforces its configured byte budget by retiring the least recently
written values, and the fetched-entry cache is bounded by entry count. These bounds
limit resource use by any single writer; they are not a Sybil defence, and an
adversary with many identities can still fill a budget with values that expire only
at the maximum time-to-live.

### Online And Onion Registries

Online-node and onion-exit descriptors are signed and expire. This bounds stale
records and makes advertised claims attributable. It does not prevent a Sybil
operator from publishing many live descriptors or many exit candidates. The routes
the privacy layer selects from these registries, and the caveat that follows from
their candidate set, are specified under [Privacy layer](#privacy-layer).

### Native Gateway

The native TUN gateway is configured by the `gateway:` section that `rings init` writes into
every new config file. Presence of that section is not consent to touch the host's routing:
a gateway starts only under an explicit `enabled: true` or `rings run --gateway`, and a section
that omits `enabled` is inert. Earlier builds treated a present section as enabled, so a config
written by hand for one of them now needs `enabled: true` to keep starting its gateway. The
generated plan assigns one RFC 6598 host address and captures no destination, so enabling it
creates the interface without steering traffic until the operator lists a prefix. The
capabilities a TUN device needs (`CAP_NET_ADMIN`, the Unix helper, a relaxed service sandbox)
are never granted by the configuration and remain an operator decision.

## Required Work Before Stronger Claims

Before Rings can claim Sybil-resistant permissionless membership, the project
needs concrete mitigation work such as one or more of:

- admission control through operator allowlists, invitations, stake, proof of
  work, or another scarcity mechanism;
- topology diversity rules for successor lists, fingers, storage owners, and onion
  route selection;
- multiple independently controlled bootstrap or registry sources;
- storage audit, challenge, or accountability mechanisms for unavailable owners;
- application-level quotas, abuse handling, and monitoring for authenticated-open
  deployments.

Any such mitigation should be tracked and reviewed as a separate design or
implementation issue. Until then, deployment documentation and feature claims
should describe Rings as DID-authenticated and Chord-routed, not Sybil-resistant.
