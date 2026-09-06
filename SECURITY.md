# Security And Overlay Threat Model

This document describes the current security boundary for the Rings overlay. It is
documentation, not a claim that every stronger model is implemented.

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
- Strong anonymity or traffic-analysis resistance for onion routes chosen from a
  Sybil-permissive live-node registry.
- Economic security, stake weighting, proof-of-work admission, globally trusted or
  portable reputation, or globally rate-limited identity issuance.

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
operator from publishing many live descriptors or many exit candidates.

### Onion Routing

Onion circuits protect payload layers from intermediate hops according to the
implemented circuit protocol. Route security still depends on the candidate set.
In an authenticated-open overlay, a Sybil operator can try to appear in multiple
route positions unless the deployment adds independent admission or diversity
controls.

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
