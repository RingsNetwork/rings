# Security And Overlay Threat Model

This document describes the current security boundary for the Rings overlay. It is
documentation, not a claim that every stronger model is implemented. Rings has two
layers with different contracts, drawn under [Layer Contracts](#layer-contracts):
the communication layer minimizes leakage; the privacy layer provides privacy.

## Summary

Rings authenticates peer identities and protocol messages with DIDs and delegated
delegatee keys. That proves control of a key. It does not make identities scarce,
expensive, or globally reputation-bearing. Peer measurements remain local advisory
state and are not portable trust claims.

The current Chord overlay must therefore not be treated as Sybil-resistant
permissionless membership. A party that can create many identities can influence
successor, predecessor, finger-table, storage-owner, and onion-route candidate
placement, and can attempt eclipse behavior.

## Assumptions

- A DID identifies a cryptographic account key, and signature checks authenticate
  control of that key.
- Delegated delegatee keys are accepted only according to the protocol checks in the
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
peers can disconnect, crash, restart, or miss heartbeats. Onion exits generate a
fresh process epoch at each start and bind it into signed descriptors and encrypted
forward layers, so reusing a persisted delegated delegatee key does not keep
pre-restart exit cells valid. TTLs, stabilization, storage repair, and descriptor
refreshes are designed for that environment.

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
`MessagePayload`, delegatee-key signatures, and the E2E ElGamal stream family. Its
contract is payload authenticity for every message and payload confidentiality for
peers that have completed the E2E handshake. It has no unlinkability contract and
cannot acquire one by changing the relay: Chord routes by the destination DID, so
every hop must see it, and every payload names its origin through the transaction
signature, so every hop can attribute it.

Every hop, and the destination, learns from a relayed message:

- the origin DID, named by the transaction signature, which is also where every report
  for the message is routed back to. A node whose requests carry a signed `reply_via`
  names its nearest linked successor there whenever it has no predecessor: while it joins, and
  again after its predecessor departs until a new one notifies it. For that time every
  hop of such a request, and of its successor or connection answer, learns one of the
  origin's links;
- the destination DID, carried by the transaction and by the relay header, because
  the next hop is chosen from it;
- its own predecessor, the authenticated transport edge the message arrived on, and
  its successor, the relay's `next_hop`;
- the encoded size and the arrival time of the message;
- nothing about the route beyond its stage: the relay carrier is `next_hop`,
  `destination`, a hop budget that every forward spends, and the delivery stage (the
  aim, which is the destination or a report's `reply_via`, and whether the route has
  been handed past it), so a hop learns its predecessor and successor and whether the
  route has crossed its aim; the destination learns only the last hop.

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

- no hop history on the wire, and no cycles. Every greedy hop moves strictly closer
  to its aim without passing it, and a route crosses its aim at most once, by a marked
  handoff after which the receiver routes greedily again but refuses a second crossing,
  ending the route with a typed error (`dht::delivery`). A route therefore takes at most
  `2|V|` hops (two greedy runs joined by one handoff, and the final hop from a
  `reply_via` peer), and a message for a node no view reaches fails fast instead of
  circling the ring. On a converged ring every greedy hop at least halves the remaining
  distance; with a sparse finger table greedy delivery degenerates to a successor walk,
  so exhausting the hop budget on the delivery path means a correct route longer than
  the budget (`2|V| > MAX_RELAY_HOPS`), not a loop. The carrier is outside every signature, so the budget bounds the work honest
  hops do for one message and is not a promise a dishonest hop keeps;
- a bounded reflection through `reply_via`: a request names one peer, and a responder
  routes at most one successor or connection answer (`FindSuccessorReport`,
  `ConnectNodeReport`) toward it; that peer receives the answer whole and hands it on
  only over a direct link to the origin. Every other report, such as `FoundEntry`
  with its entry data, is routed straight to the origin, so a small request cannot be
  turned into a large report aimed at a third party;
- no telemetry in the envelope beyond what routing needs: the next hop, the
  destination, the hop budget, and the delivery stage;
- payload bytes encrypted to the destination's account key once the E2E handshake
  has completed, with the signed envelope supplying the integrity that the
  malleable ElGamal frames lack on their own;
- every signature bound to `network_id` and to a per-message-family domain tag, so
  an observation in one overlay is not a credential in another.

Delegation references do not change what a hop learns. A link sends each delegation
inline until the receiver confirms it, and its 20-byte content address
(the trailing bytes of `keccak256` over the encoded `Delegation`) afterwards; the
receiver resolves the address to the exact delegation that would have travelled
inline, then verifies both proofs as before. The address names the whole
delegation, not the delegatee key, because a delegatee key can carry several
delegations and any delegator can sign one for a key it does not hold. The cache
behind the references is scoped to one direction of one admitted connection
generation and obeys these rules:

- it is populated only by the peer at the other end, with delegations of frames that
  verified on the link (a frame from any other peer, or on a callback bound to no
  handshake, is judged self-contained and a reference in it refused) or with an
  announcement this end asked for and whose delegation verified; an unsolicited
  announcement is ignored, so an unauthenticated party cannot fill it; what a
  verified frame teaches is applied before that frame is gated, so a held frame
  never waits on the fate of the frame that taught it;
- the sender references only what the receiver confirmed, so loss or reordering on
  the link costs inline frames and never a stall; the link is treated as a datagram
  link throughout;
- the sender remembers at most 64 delegations and the receiver 128, under one
  least-recently-referenced order over the frames both ends saw, so on a lossless
  link the sender goes back to inline before the receiver could have forgotten;
  both tables are scoped to the connection generation;
- a miss (a frame lost between the two orders, the two ends disagreeing on expiry,
  or a misbehaving peer) holds that frame alone, at most 16 per connection and for
  at most twice the delivery timeout plus one period of the inbound actor's sweep,
  and is repaired on the link by one unsigned request per missing delegation of a held
  frame (or of the oldest held frame, when a frame finds the hold full) and exactly
  one unsigned answer per question; each control frame is emitted on the connection
  generation it was judged on, in a task of its own rather than from the transport's
  read loop, and at most 128 of them are in flight to one peer, twice the raw frames
  that peer may have in flight at this end's transport, so their cost is bounded by
  the frames accepted from the peer; a frame the peer was asked about and did not
  back (disclaimed or invalid announcement, hold timeout, failure on release) is
  charged to the peer as a receive failure, while a frame that finds the hold full,
  or whose question this end never managed to send, is dropped uncharged, as one the
  pre-admission hold cannot take is; a frame released after its connection generation
  was superseded is dropped, never delivered; no hop asks the origin for anything;
- an expired delegation is evicted, a reference to it is a miss, and re-announcing the
  expired delegation is refused exactly as it is inline.

### Transaction replay boundary

Signed transactions use a destination-scoped sequence stream keyed by `network_id`, the origin
account DID recovered from the delegated delegation, and the final destination DID. The final
destination persists a fixed 32-sequence acceptance window before application validation and
handler dispatch. Exact duplicates, conflicting transactions at one sequence, and sequences
below the retained window are rejected as separate typed verdicts. Delegation-key rotation does not
reset the account stream, sender timestamps do not order it, and intermediate Chord relays keep no
origin replay state.

This is an at-most-once dispatch guarantee only while the replay store is retained. A crash after
the receiver commits a sequence but before handler dispatch can lose that event; replay storage
and application effects are not one transaction, so exactly-once effects are not claimed. A gap
is allowed and proves neither omission nor relay fault. Deleting the replay store deletes the
guarantee, and there is no reset/incarnation protocol: capacity and persistence failures fail
closed instead of evicting an old stream. Native daemon and browser-provider defaults use durable
stores; custom builders must supply `ReplayStorage` to preserve the guarantee across restart.
Concurrent devices or processes for one account/destination must delegate to one serialized
allocator; sharing only the underlying store is not an atomic multi-writer protocol and may
produce a typed fork.

### Final-destination origin rate boundary

Transport admission retains separate node-wide and per-immediate-peer count and byte occupancy
bounds. Those bounds limit queued memory; they are not rate limits and do not identify the signed
origin. After both payload signatures verify, a final destination additionally applies two
runtime-local token buckets keyed by `(network_id, origin account DID, destination DID, logical
inbound lane)`: one for messages and one for verified logical-message bytes. The account DID comes
from the inner transaction signature, so changing a delegated delegation or last-hop relay does not
reset an active allowance, while unrelated origins carried by one relay remain independent.

The destination serializes replay classification and quota admission as one commit boundary. A
Replay, Fork, or Stale verdict consumes no tokens; a quota rejection does not advance replay; and
a replay-persistence failure rolls back the provisional quota reservation. After that commit,
logical lane capacity and application validation run, so even an application-rejected transaction
has consumed quota for the authenticated work it caused. The complete order is: raw transport
occupancy, decode and both signature checks, final-destination check, replay plus quota commit,
logical lane reservation, then application validation and handler effects. An over-quota
transaction therefore enters neither the logical mailbox nor application code.

The byte charge is `Transaction.data.len()` from the verified original transaction. A normal
frame is charged once at destination admission. Chunk envelopes are transport framing and consume
no origin quota; after complete reassembly and signature verification, the recovered original
transaction is charged once by the same function. Quota time is monotonic and local, token state
is never serialized into the replay snapshot, and each lane has a hard record bound. Under
pressure only a fully replenished idle record is reusable; if none exists, admission fails closed.
Quota drops are counted by the bounded lane and reason dimensions, never by origin DID. They are
local drops and do not disconnect the immediate peer, which may be an honest relay.

### Provisional service-receipt boundary

The implemented receipt protocol is a provisional `Probe` evidence collector, not the
finalized DRanking ledger. A beneficiary sends an authenticated request containing a fresh nonce
and a current or adjacent five-minute epoch. The provider returns an offer that embeds the exact
signed request transaction and an exact provider-signed completion transaction. The provider and
beneficiary then sign the same canonical claim under different role domains, and the beneficiary
returns the complete receipt in an acknowledgement. Account DIDs define the roles; delegated
delegation rotation neither changes a role nor creates a distinct receipt identity.

The wire markers and signing domains are protocol-domain separators, not compatibility
fallbacks, and carry no version: the protocol is not versioned before 1.0. Only `Probe` is
accepted. Unknown service kinds, noncanonical bytes, a unit count
other than one, same-account roles, digest or role mismatches, stale epochs, and expired delegated
proofs fail closed during live admission. The old unsigned liveness probe/report wire is removed.

Cryptographic validity and live collection are intentionally separate. A stored receipt can later
prove that both account roles signed one canonical claim and that their delegated proofs were valid
at signing time. It cannot later prove that the receipt was observed inside the receiver's live
epoch tolerance. Live admission checks that fact once and stores the local observation time.

Local measurement writes require explicit attribution-aware events or atomic batches;
there is no counter-only fallback that fabricates zero-byte events or hides update errors.
Every retained peer projection carries its credit record. Reliability policy and byte credit
remain separate, and provisional receipt admission cannot modify either one.

IndexedDB reads remain atomic read-and-touch transactions: successful reads take the next
value of a store-wide logical access clock as the row's LRU stamp, advance the clock in the
same transaction, and await commit. Stamps are distinct and strictly increasing, so eviction
order does not depend on browser timer precision. Opening a schema-version-1 database migrates
it in place, preserving every row and its former eviction order; the adapter does not erase
user data to change the schema.

Provisional evidence is isolated from peer measurements, `CreditRecord`, and
`order_peers_by_quality`; it changes neither routing nor credit. The evidence store has hard global
and per-provider/beneficiary record and byte limits, deterministic oldest-first eviction, aggregate
unlabelled counters, bounded digest pagination, and a separate durable snapshot on native and
browser nodes. Invalid persisted entries are skipped independently so one malformed record cannot
hide valid evidence. Fixed-size replay markers have separate global and per-beneficiary count
bounds. Receipt eviction reduces later evidence availability but leaves its replay marker intact;
if marker capacity is exhausted, new keys fail closed instead of displacing live replay state.
Browser provider construction also fails closed if its dedicated IndexedDB evidence store cannot
be opened; it never silently substitutes process-local memory for the crash-recovery assumption.
Provider/measurement constructors without an explicit durable evidence backend leave receipt
collection disabled: probes still serve liveness, but evidence admission cannot return success.

Receipt admission and its replay marker are committed to the separate evidence storage before the
admission returns success; a storage failure rolls the in-memory transition back. Consequently a
hard process crash cannot turn a successfully returned admission into a fresh key after restart.
The monotonic replay floor also prevents a wall-clock regression from reopening an epoch whose
evicted markers were pruned. Explicit deletion or replacement of the evidence store resets this
local history and therefore starts a new collector state; that administrative action is outside
the process-crash guarantee.

These receipts do not prevent colluding accounts from manufacturing mutually signed probes, make
DIDs scarce, prove useful relay/storage work, or create Sybil-resistant reputation. A future
finalized-epoch DRanking receipt must use a distinct canonical format and signing domain; it must
not reinterpret this provisional wire or silently aggregate it into trust.

A leak on this layer is a communication-layer bug. Cover traffic and circuits do not
fix it: they run above the relay and inherit whatever it exposes.

### Privacy layer

The privacy layer is `crates/node/src/onion`: layered ElGamal-AEAD circuits over
direct edges, fixed-batch cover cells with pacing, replay witnesses, fixed size
classes for cells, and route selection from the online-node and onion-exit
registries. It sits in `rings-node` deliberately: Chord remains the storage and
discovery substrate, and exit policy is an application decision.

**Per-hop knowledge bound.** Forward layers are wrapped from exit to entry with the
selected hops' delegation public keys. Each relay decrypts exactly one ElGamal-AEAD
layer and learns only the immediate next hop plus an opaque inner layer; backward
frames carry a client-encrypted AEAD payload that relays forward with local return
state. A circuit id identifies exactly one directed edge of one route and is
rewritten at every hop, and the client/exit return id is encrypted inside the exit
layer and never appears as an edge header. The final layer also authenticates the
selected descriptor's random process epoch, and the exit checks that epoch before
emitting any adapter effect. A relay therefore knows its predecessor and its
successor on the circuit and nothing else about the route; only the exit sees the
application payload, and only the client knows the whole route. A route is at most
eight hops and defaults to three, counting the exit.

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

An onion-exit descriptor signs exactly one canonical service name, its policy,
node type, network, process epoch, timestamps, and signer material. There is no
parallel transport enum or descriptor schema number: native exits currently serve
the advertised names through the TCP exit runtime, including the reserved `https`
name. A new incompatible descriptor shape is therefore a network-wide release
cutover, not a value negotiated inside the descriptor. Route construction enters
through the policy-aware selector only: proxy protocol, target policy, entry guard,
and direct-exit admission are explicit predicates rather than permissive wrapper
defaults.

TCP and HTTPS exit adapters share one process-local forward-nonce replay witness.
The authenticated service name still binds the adapter action, but replaying the
same `(peer, circuit, nonce)` through another installed service cannot authorize a
second action. This witness is deliberately process-local; the signed random
process epoch invalidates cells created for a previous process generation.

**Entry guards.** A client pins a small local set of eligible first-hop relays per
network and persists it outside Chord. New routes choose the relay first hop only
from that guard set, so repeated HTTP requests, CONNECT tunnels, and TCP/UDP onion
flows do not resample the whole live relay population as their entry. Guards are
replaced only when they are no longer live, no longer satisfy the caller's first-hop
policy, or a healthier eligible replacement set exists. This reduces the number of
relays that can observe the client edge over time, but it does not hide the client
from whichever guard is selected for a given circuit.

## Feature Boundaries

### DID Identity

DID signatures authenticate the key behind a message, descriptor, or delegation
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
What a relayed message reveals to each hop is stated under the communication layer
contract above.

The decoders that admit relayed bytes have generated decode-boundary smoke tests in
their owning crates. `rings-core` covers the base58-check envelope, postcard wire
envelope, message body decode, chunk framing, and chunk reassembly bounds;
`rings-transport` covers SDP `a=max-message-size` parsing; `rings-node` covers
the JSON-RPC body decoder and authorization classification. CI generates each run's
seed and case count, so the repository does not carry generated corpora.

### Transport lifecycle boundaries

Transport cleanup uses the generation-pinned `close_connection_if_current` and
`Pool::safely_remove_if_current` boundaries. A delayed close must not resolve a CID
again and retire its replacement. Raw inbound frames retain callback-instance
identity; matching that private identity also proves the immutable peer attached
to the capacity lease, including when two callbacks share a peer ID.

Each native or WASM send has one close actor with exclusive state and physical-close
ownership. `SendLifecycle` shares only observation rights, a bounded mailbox
address and the synchronous generation fence. Failure observed after irrevocable
admission fences before producing the sealed actor command. Pure reducers govern
failure and close transitions; duplicate commands cannot initiate close twice for
that send, and late acceptance cannot reopen retirement. Distinct concurrent sends
may still close the same generation; explicit close remains generation-pinned.
The actor publishes distinct unused, succeeded, failed and interrupted outcomes.
Finite-state exploration checks safety within a one-send/two-observer abstraction;
actor conformance and lifecycle regressions check the IO boundaries. Both backends
use `core::send` for the lifecycle, resource owner, checked byte accounting and
executor-neutral actor. WASM uses a one-shot mailbox and `spawn_local`; its fence
is synchronous, while the actor invokes browser close in a later microtask. Both
backends wait for a requested cleanup's terminal result before returning a send
error; cancelling the waiter does not cancel cleanup. Browser close success means the local API returned, not remote acknowledgement. Native
first-poll locking and timeouts remain platform effects. Abort/trap or browser
context destruction cannot guarantee cleanup or delivery of a terminal snapshot.

`OwnedSend` retains the primitive and `QueueSend` retains its channel lease and
acceptance proof outside the primitive's async stack. The first-poll boundary
releases the admission lock before reporting failure. Timeout, panic, or caller
abandonment fences new sends before dropping the owner's captured resources.
A pending continuation and already-started physical cleanup outlive their caller;
revocable or accepted caller cancellation does not trigger retirement.
`NativePhysicalCloseWitness` separately records successful physical close.
Cleanup-task termination during executor shutdown is not evidence of physical
close, and a stopped executor cannot guarantee completion of asynchronous cleanup.
Backend-free/default/dummy notifier timers retain the runtime-independent thread
scheduler. These contracts are not removed by the transport API cleanup in #787.
ICE configuration accepts password credentials; unsupported OAuth values are
rejected rather than silently downgraded by the native backend.

Relay socket and WebTransport tables treat the pure reducer as the duplicate-open
authority. If a duplicate effect nevertheless reaches an occupied key, the engine
refuses the new insertion and preserves the live handle and its generation; it does
not cancel and replace the committed resource defensively.

### Outbound scheduler ingestion and cancellation

Native and WASM share futures channels and the existing `TransferQueues` reducer.
Collected submissions retain their permits, bounding ingestion to 256 even with
concurrent producers. One notification slot coalesces `CancelStopped`; each drain
reads it once and scans at most 256 transfers. Control submitted before the FIFO
drain is visible before selection; a submission racing the empty read may enter
the next iteration. There is no fixed cutoff leaving earlier control behind bulk.
The 4:1 burst and lower-lane rotation are unchanged: a continuously runnable lower
class receives service within 15 charged admissions/failed attempts, assuming
executor/gate service and delivery, timeout or cancellation progress.

Receipt frees the notification slot before scanning; scans never read ingress.
Shutdown releases all batch ownership before publishing its collected results.
Common native/browser regressions cover these boundaries; native threads also
exercise submission/close contention. The existing queue model checks 13^6 traces
of six actions with eight slots, not arbitrary-schedule liveness. The former drain
bounded submissions but not repeated notifications; this is not a claim of
observed starvation or a wall-clock bound.

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
sections above. The body decoder and the classification are covered by generated
decode-boundary tests whose invariant is that a body that does not decode, an invalid
call, or an unknown method never classifies as public.

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

Gateway runtime construction validates configuration before allocating packet resources.
Standalone server and TCP stack construction use the same validation boundary. Platform
controllers still validate their own plans before host effects. Exit loss is a degraded health
projection of the active lifecycle, not an alternate packet-admission state. The TCP endpoint
index owns socket membership; released endpoints cannot access a recycled socket handle.
Typed drop/rejection reasons are available at debug tracing level without packet contents or
flow addresses.

### Controlled Webview Requests

The host supplies one trusted `source_target` from controlled frame state. The webview crate
derives its origin for CORS and credentials and its site for cookie policy; page-supplied
headers cannot supply a competing origin. The complete source URL remains available for
existing scoped diagnostics. CORS author-header selection shares the request privacy
allowlist and excludes gateway-generated Origin, Cookie and Accept-Encoding metadata.

Response limits remain enforced immediately after each transport send, including preflight
responses, even if a transport ignores its supplied limit. A further limit after rewriting is
required: URL expansion and bootstrap injection can grow HTML/CSS beyond the original body
size. None of these checks is replaced by trusting a transport or a Content-Length header.

## Required Work Before Stronger Claims

Before Rings can claim Sybil-resistant permissionless membership, the project
needs concrete mitigation work such as one or more of:

- admission control through operator allowlists, invitations, stake, proof of
  work, or another scarcity mechanism;
- topology diversity rules for successor lists, fingers, storage owners, and onion
  route selection;
- multiple independently controlled bootstrap or registry sources;
- storage audit, challenge, or accountability mechanisms for unavailable owners;
- application-specific authorization, abuse response, and monitoring beyond the generic
  final-destination origin quotas for authenticated-open deployments.

Any such mitigation should be tracked and reviewed as a separate design or
implementation issue. Until then, deployment documentation and feature claims
should describe Rings as DID-authenticated and Chord-routed, not Sybil-resistant.
