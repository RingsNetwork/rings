# Security And Overlay Threat Model

This document describes the current security boundary for the Rings overlay. It is
documentation, not a claim that every stronger model is implemented.

## Summary

Rings authenticates peer identities and protocol messages with DIDs and delegated
session keys. That proves control of a key. It does not make identities scarce,
expensive, or reputation-bearing.

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
  secret, because a peer can choose to use the same value.
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
- Economic security, stake weighting, proof-of-work admission, reputation, or
  globally rate-limited identity issuance.

## Feature Boundaries

### DID Identity

DID signatures authenticate the key behind a message, descriptor, or session
delegation. They do not prove that two DIDs are controlled by different operators,
and they do not prevent an operator from generating many DIDs.

### Chord Routing

Chord routing assumes the node set is acceptable under the deployment model. It
gives deterministic routing over the observed topology; it does not defend, by
itself, against an adversary that can occupy many positions on the identifier ring.

### DHT Storage

Storage ownership and replication are topology-derived. CRDT joins, owner checks,
TTL cleanup, and read repair improve convergence among honest or fail-stop peers.
They do not force a Byzantine storage owner to serve data or preserve data it has
chosen to withhold.

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
