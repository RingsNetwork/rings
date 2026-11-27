# DHT

The Rings Network leverages the Chord algorithm for its DHT implementation. The Chord algorithm is a protocol for lookup in a peer-to-peer distributed system that allows nodes in the network to find the location of data. It enables effective routing of messages and storage of key-value pairs in a peer-to-peer setting, guaranteeing high availability in the Rings Network.

In the Rings Network, nodes sit on a 160-bit ring (`Z / 2^160 Z`), where each node maintains a successor list (`K = 4` by default), predecessors, and up to `160` finger-table slots (bounded at `330` admitted connection slots). This design maximizes the functionality of the Chord algorithm for quick and effective participant lookup and location.

## DHT

A DHT is a class of decentralized distributed systems that provide a lookup service similar to a hash table; key-value pairs are stored in a DHT, and any participating node can efficiently retrieve the value associated with a given key. The main advantage of a DHT is that nodes can join or leave the network with minimal disruption.

## Chord

Chord is a protocol and algorithm for a peer-to-peer distributed hash table. It specifies how keys are assigned to nodes, and how a node can discover the value for a given key by first locating the node responsible for that key.

Chord organizes nodes into a circular ID space, often visualized as a ring. Each node in the network has a unique identifier, and each data item identified by a key is assigned to the node whose identifier is closest to the key (according to a certain distance metric). When a node needs to find the value associated with a key, it uses the Chord protocol to locate the node responsible for that key. This is done using a "finger table" that each node maintains - a sort of routing table with pointers to other nodes in the network. This structure allows for efficient routing of queries to the appropriate node.

Chord provides a fast, distributed lookup method that scales logarithmically with the number of nodes. That is, even as the network grows large, the number of steps required to find the node storing a particular key remains relatively small. This makes it very efficient for large-scale distributed systems, where resources might be spread across many nodes and those nodes may be constantly joining or leaving the network.

## Correct Chord

Correct Chord is derived from Pamela Zave's work on Chord, and it encompasses two core principles:

* In classical Chord correctness proofs, tolerating `r` simultaneous ungraceful crashes requires a ring with at least `r + 1` live members (`r` being the successor-list capacity). Rings operates seamlessly at any ring size from `N = 1` upward, preserves state across voluntary departures via acknowledged `Leave` carrier handoff (`#1057`), and adapts replicated custody to `min(r, live_holders)` on small overlays while guaranteeing full `r`-way crash resilience once `N ≥ r + 1`.
* The Chord Paper defined the maintenance and use of `finger tables`, which improve lookup speed by providing pointers that cross the ring like chords of a circle. **Because finger tables are an optimization and they are built from successors and predecessors, correctness does not depend on them.**

Rings Network builds upon Correct Chord and incorporates several modifications, including support for multiple successors and improved stabilization algorithms, among other enhancements.

## Delivery toward a node

Finding the owner of a key and delivering a message to a node are different questions, and Rings
answers them with different rules. The owner lookup (`find_successor`) answers `head` when the key
lies between a node and its successor head, and crossing the key is correct there: the owner is the
first node at or after it. A node is not a position, so a message for a node must never pass it.

Delivery (`dht::delivery`) works as follows:

1. **Greedy.** Each hop forwards to the known peer (successor or finger) that lies between itself
   and the destination and is nearest the destination. The clockwise distance strictly
   decreases, so no route can cycle. A hop linked to the destination delivers directly.
2. **One handoff.** When no linked known peer lies on that arc, the hop hands the message once to
   the first linked known node after itself, which lies past the destination, and marks the
   carrier. A node with a sparse view, such as a leaf linked only to its guard or a joiner linked
   only to its bootstrap, hands off on its first hop, and the receiver's fuller view routes on
   greedily. A second crossing is refused and ends the route with a typed error
   (`RelayDestinationUnreachable`), so repeated crossings can never circle the ring.

A message for a node that no view reaches therefore fails fast, within at most `2|V|` hops,
instead of circling the ring until its hop budget runs out; it is delivered once the ring has
converged. Greedy hops choose only peers this node is linked to, because the successor list may
name peers a stabilization report introduced before any connection to them exists.

A node without a predecessor is the one case that cannot wait for convergence: a joiner needs the
answers to its own join lookup and connection offer to converge at all. Whenever a node has no
predecessor, its requests carry a signed `reply_via` naming its nearest linked successor, which is
its bootstrap during a join. Successor answers (`FindSuccessorReport`) then travel to that peer,
which hands them over its direct link. Every other report goes straight to the origin, so
`reply_via` reflects at most one bounded answer per request. A connection answer needs neither:
it retraces the path its offer recorded, as the next paragraph describes.

Connect signaling does not wait for convergence either. A `ConnectNodeSend` records the
relays it crosses, outside every signature and bounded by the hop budget, and its answer retraces
them, each hop a link the offer itself just crossed, instead of being routed afresh over a view
that may have just dead-ended. When two nodes offer to each other at once, an offer still being
created yields, since nothing has crossed yet. Of two sent offers the smaller DID's prevails: the
larger DID abandons its own and answers, and the smaller DID refuses the larger's and sends its
own offer back along the refused offer's path, under the transaction it first left under. The
answer echoes that transaction and applies only to the offer that left under it. The edge is
therefore established whenever either overlay offer is delivered.

## Non-transitive connectivity & passive gap-bridge hints

Real overlays include nodes behind heterogeneous NATs or symmetric firewalls where connectivity
is non-transitive (for instance `A` reaches `B` and `C`, and `C` reaches `D` and `E`, without
full mutual edges across clusters). Two mechanisms prevent silent routing blackholes:

- **Multi-predecessor retention (`TopologyState::predecessors`)**: alongside the primary
  predecessor, each node retains bounded secondary sub-ring predecessors ordered by clockwise
  distance and prunes stale entries immediately when a strictly closer predecessor joins or a
  two-node ring expands.
- **Passive 1-hop gap-bridge hints (`TopologyState::bridged`)**: stabilization and admitted
  neighbor reports record bounded 1-hop bridged peer hints (`2K` per reporter, `2K²` total),
  which `delivery_step` consults alongside successors, predecessors, and fingers without issuing
  unnecessary connection churn.

## Voluntary `Leave` & relay carrier handoff

When a node shuts down cleanly (`Swarm::leave`, `Provider::leave()`, `SIGINT`/`SIGTERM` on native
daemons, or best-effort `pagehide` in browsers), it avoids forcing peers to wait out the 45-second
liveness timeout:

1. **Relay carrier handoff (`hand_off_relay_carriers_for_leave`)**: if the departing node holds
   offline relay-inbox carriers, it hands them to its next live holders via
   `SyncEntriesWithSuccessor` and waits for acknowledgements before removing local state.
2. **Authenticated `Message::Leave`**: the node transmits `Leave { successor_list }` to its
   `leave_targets()` (primary predecessor, successor head, and secondary predecessors). Receiving
   peers remove only the authenticated sender and promote routable successor replacements from the
   supplied list.

## Data-topic storage

A data topic is a replicated set of elements (a state-based CRDT): each element carries the
*dot* of the write that issued it, and replicas and hand-offs merge by join, so a replica's value
does not depend on the order writes and hand-offs arrive in. A reader's fetch cache merges too:
it joins every reply it receives, so a late reply from a stale replica cannot replace a
fresher one.

A fire-and-forget write returns once it has been applied locally or has left for each of its replicas, whereas workflows requiring confirmed replica persistence use acknowledged store-report writes (`ScopedStorage::storage_extend_reported`, as used by [`rings-mail-gateway`](../mail-gateway.md)). Because ring views evolve as peers join and stabilize, an owner hands its replica to a
new owner in its next storage repair round. The writer itself applies its
own write to its fetch cache immediately, so it reads its own writes at once.
It also stamps each write after every write it stamped before and every version it
read for the key, at most the 3 s clock tolerance ahead of its clock, so its
successive writes to a topic are listed in the order it issued them, even two in one
millisecond or across a step back of its clock. Any other node reads the write once its lookup
reaches an owner that holds it; external readiness checks therefore verify publication from another
node's view or via `storage_extend_reported` rather than relying on the writer's local cache alone.

### Authenticated CRDT provenance

Every `EntryKind::Data` mutation is cryptographically bound to its author under
`crates/core/src/dht/entry/provenance.rs`:

- **`SignedDataMutation` (`rings-core:data-entry:mutation`)**: writes sign a fixed-size Merkle
  batch claim (`DataMutationClaim`: `Reset | Append | Retract`) binding author `did`, version,
  `leaf_count`, and `merkle_root`, keyed per-actor via `DataProofKey` (`Register`, `Add`,
  `Remove`).
- **`RetractionWitness` & fair per-writer retention**: removals verify against `RetractionWitness`
  attestations, preventing one writer from forging or retracting another writer's entries, while
  cap eviction enforces fair per-writer retention so one active writer cannot crowd out others.

- **Every element expires individually.** An element expires at the earlier of two instants:
  100 minutes after its dot was issued (the element horizon `H`, the longest retention one write
  may request), and its topic's retention bound, the latest bound any write joined into the
  topic requested (a plain write, such as `publishMessageToTopic` or `registerService`,
  requests 10 minutes; an extend, `storage_extend` in core, requests the lifetime its writer
  names, up to `H`, for several elements in one write, as the [mail gateway](../mail-gateway.md)
  does). Writing the value again issues a fresh dot and a fresh bound and is what
  keeps it: a sole writer rewrites each value within 10 minutes, and every writer within `H`,
  even while other writes keep the topic alive. The online-node, onion-exit and DNS-relay
  registries re-append their descriptor every heartbeat (30 s by default): a signed 10-minute
  lease whose bytes stay fixed, so a heartbeat raises its dot and writes no tombstone.
  Once the topic's bound has
  elapsed, it serves no element; it is kept only while it holds a tombstone or overwrite that
  is not yet collected, and it answers lookups as absent, from a replica and from a reader's
  fetch cache alike, so a reader asks the next replica. Its tombstones still spread by
  hand-off, by republish, and by read-repair from the reader's cache, which repairs a missed
  replica from the carrier it holds even past its bound.
- **Actor-scoped removals win over earlier writes by the same author.** On `EntryKind::Data`, each
  tombstone is cryptographically authorized (`RetractionWitness` / `SignedDataMutation`) and scoped
  per `(element_digest, author_did)`, covering that author's dots of the value up to the tombstone
  dot (`co_dots` preserves concurrent add witnesses across distinct writers). One writer's removal
  or clock skew never suppresses or retracts another writer's concurrent or earlier write on the
  shared topic, while sequential rewrites and retractions by the same author commute cleanly in HLC
  dot order.
- **Removals are collected by the clock, not by a reset.** A tombstone is collected `H + σ`
  after the dot it covers, where `σ` (3 s) bounds clock skew between nodes, and an overwrite's
  reset `H + σ` after it was issued. By then every copy of every write they shadowed has expired
  on every node, so no stale replica can bring the value back, and a topic that holds either
  stays stored until it is collected. Tombstones live in a store of their own that never evicts:
  the data store's byte budget drops the values of a topic, never its tombstones, and
  each tombstone is charged to the connection slot of the connection that delivered it, or to
  the node for its own writes and repairs: each slot has a floor of 128 uncollected, the
  rest share a pool of 16384 in fair claims, and the node keeps 7168 of its own. A hand-off of
  the range a node now owns, from the node its view names as the range's previous owner, draws
  on a custody share of 16384 instead, so a busy range is never capped by one neighbour's slot.
  No number of writers or connections crowds out a slot below its floor. There is no compaction
  operation: a reset stamped by one owner would erase every concurrent write that owner had
  not yet received.
- **Bounds.** A topic holds at most 1024 elements (the oldest are dropped when the cap binds) and
  at most 1024 tombstones not yet collected. The tombstone bound refuses, it never
  drops: a removal, a replication, or a repair whose result would hold more is refused and
  changes nothing, since a dropped tombstone could still cover a live write somewhere, and a
  delta carrying more is not admitted. This admits about 10 removals a minute per topic,
  sustained; an application that removes faster shards its topic (each value to one of `k`
  topics by its hash). A full topic, removals included, encodes in at most 33.83 MB, which fits
  one transport message. Registry heartbeats remove nothing, so a registrant leaves at most 13
  descriptors (one per lease) within the horizon and no tombstone; the cap drops superseded
  leases first, since no heartbeat refreshes their dots.

An overwrite (`storage_store` / `DataMutationOpKind::Reset`) advances an authenticated per-author
reset floor (`register` / `co_registers[actor]`), cleanly replacing that author's prior elements
and tombstones on the topic while leaving every other writer's elements and tombstones intact.

## Relay inbox

A message for a peer that has no connection is held for it: the node responsible for the peer's
position (the first node at or after it) signs the message and writes it into the peer's inbox,
a relay carrier at the key `peer + 1` in a store of its own. Relay mail is kept only in a
retained relay store, so a node built or run without one (a browser whose IndexedDB
cannot open, a library node built without one) declares, in every connection handshake and in
its online-node descriptor (`relay_custody`), that it holds no relay mail. Such a node still
joins the ring, routes, sends and receives online messages, and owns ordinary DHT data; only
relay custody moves past it.

A held message expires 7 days (`MAX_RELAY_INBOX_TTL_MS`) after the instant its holder signed,
however long newer holds keep its inbox alive: every read, join and sync drops it, and a
recipient never delivers it. The rule reads the signed hold instant alone, so every replica
drops the same messages and a member cannot extend one by writing it again.

The **holder** of an inbox key is the first node at or after the key's owner, along the
successor path, that holds relay mail:

```text
holder(k) = first n in [owner(k), succ(owner(k)), succ²(owner(k)), …] with holds(n)
```

One pure function, `dht::relay_holder::relay_holder`, decides it on each node's view, and every
relay path uses it: the hold, the recipient's removal of what it delivered, the hand-off join and
its acknowledged cleanup, the repair pass that lists what to hand off, and the recipient's
drain. So a writer and a reader whose views cover the same nodes always name the same holder.

The carrier is **replicated** to the key's **members** ([Rings whitepaper](https://rings.rs/papers/rings/), Durable Local Stores & Replicated Custody): the
first `r` nodes from the key's owner that hold relay mail, the holder first, `r` configurable and
8 by default (`RelayReplication`). The members form a chain in ring order, and a held message is
lost only if every member holding it is lost within one repair delay (Propositions 10 and 11), so
a member's tab closing or its private session ending costs nothing while another member
survives.

- **Age gate.** A node with a relay store declares that it holds relay mail only once it has
  been online for 2 minutes (at least four stabilization periods). Until then it routes and
  fetches its own mail, but is no member, so a node that leaves within its first minutes never
  pays a member's copies. When its gate opens it declares the change to its connected peers
  (`DeclareRelayCustody`); later peers read it in the handshake.
- **Chain replication.** The holder takes a hold and forwards it byte for byte to its next
  member (`ReplicateRelayHold`), which forwards it on to the chain's end. Acknowledgements return
  from the end, each member acknowledging the member it came from once the rest of the chain
  did, and the holder acknowledges the writer last: an acknowledged hold is held by every member,
  `r` of them, or every holder of a ring with fewer. A forward sent again, on the writer's resend,
  carries the hold's digest alone.
- **Next member.** A member's next member is the first holder after it. It is in the member's
  successor list, or past it: then the member asks the owner of the key just past its list
  (`FindRelayHolder`), which names the first holder at or after it, or the last node it places to
  ask past again, and the member caches the answer until its successor list changes.
- **Chain repair.** Every carrier a member keeps has the same chain neighbours, its previous
  holder `p₁` and its next holder `n₁`, but at the chain's ends, so each repair pass sends one
  digest set to each (`RelayCarrierDigests`), and a neighbour asks only for the carriers it lacks
  or holds otherwise (`RelayCarriersWanted`): at most two repair messages per node and pass,
  whatever the carriers it keeps. A node that keeps a carrier and is no member any more hands it
  to the members and deletes its copy only after a member acknowledges holding it.
- **Event-driven repair.** The loss of a chain neighbour, by a closed connection or a failed
  liveness check, wakes a task beside the maintenance loop that runs the chain exchange at once,
  outside the repair schedule's admission window, which guards bulk storage sync and not these
  two digest messages. So a lost member is replaced within its detection time plus `r` hops; the
  periodic pass is the backstop.
- **Chain liveness.** A member probes its two chain neighbours every 2.5 s, on the same probe
  clock as every peer, and suspects one whose probe is 7.5 s unanswered. A suspect is no
  chain member until it answers on time twice in a row, and both the suspicion and the clearing
  run the chain exchange at once; suspicion evicts nothing, eviction stays at the 45 s liveness
  deadline. So a silent loss is detected within 10 s, and a slow but live neighbour costs a chain
  shift and back, never a copy: a neighbour whose answers hover around 7.5 s changes the chains
  at most twice per 12.5 s. The probes cost each node four messages per 2.5 s, whatever `r`.
- **Who is a member.** A node decides membership and its rank on its own view: it is a member of
  `k` iff `k` lies in `(succ(p_r), succ(self)]`, `p_r` the `r`-th previous holder behind it. A
  copy is admitted only from its previous or its next member.
- **Fetch and removal.** A recipient that holds relay mail reads its inbox from its own store;
  one that does not fetches it from its members, the holder first and then down the chain to
  the next it can reach (`FetchRelayInbox`, answered only to the recipient itself), and removes what it
  delivered at the member it read it from. No report restricts relay custody: a lagging
  member's copy, or a new holder's before its hand-off lands, may miss an element that still
  exists.

A view covers the node's predecessor, the node, and its successor list. A holder also needs the
keys owned on the storage-less runs behind it, whose owners lie behind its predecessor: every node
passes to its successor head, each repair pass, the list of its `r` nearest *previous holders*
(itself and the nearer ones its predecessor passed it when it holds relay mail, otherwise what
its own predecessor passed it), and takes that list from its predecessor alone. The list crosses
each run one link per pass, so every holder learns where the runs behind its previous holders
start, however long they are. A list that comes round the ring names every holder, and the view
then searches round the ring for members.

- **No holder, no hold.** When no node from the key's owner to the end of the writer's
  successor list holds relay mail, the writer refuses the hold itself with
  `RelayHolderUnavailable`. A node that receives a relay carrier its own view does not make it
  hold refuses it with `RelayCarrierNotHeldHere` rather than pass it on, so two views that
  disagree cannot bounce it.
- **Kept until acknowledged.** A writer keeps every hold it sends until the holder it sent it
  to acknowledges taking it (`RelayCarrierHeld`), and sends every hold still kept again on each
  repair pass, to the holder its view then names, until a holder takes it or the message's
  sender proof expires (10 minutes by default). A resend is byte-identical, so a holder that
  took it already only acknowledges it again. A refusal (`RelayCarrierRefused`) only lets the
  writer send sooner, so a lost acknowledgement or refusal costs a resend, never the mail.
  Both name the hold by its digest and count only from the node it was sent to. A writer keeps
  at most 64 holds unacknowledged and refuses another to its local sender
  (`RelayHoldsUnacknowledgedFull`) rather than forget an earlier one.
- **Convergence.** A peer's declaration arrives in its handshake, before it is admitted, so a
  view places every neighbour with its declaration. The previous-holder lists settle within
  `r(L + 1)` repair passes after a membership change, `L` the longest storage-less run. Until
  then a hold lands where repair moves it on, or is refused and sent again, so no mail is lost
  in the window, including when several relay-capable nodes join one long run within it.
- **Who may hold.** The holder admits a hold only from the node responsible for the
  destination as its view places it: its successor head for a destination it owns, itself for a
  destination in `(predecessor, self]`, and otherwise a storage-less node of the run at or after
  the destination.
- **Membership changes.** The holder moves only when the first relay-capable node after the owner
  changes. A storage-less node joining or leaving between the key and the holder moves nothing. A
  storage-less node joining as the key's owner moves custody forward to the next holder; a
  relay-capable node joining inside a run takes custody back from the holder after it. Each move is
  an ownership hand-off acknowledged by the new holder before the old one deletes. A carrier whose
  owner the old holder cannot place is offered to both ends of its view and to the holder the key's
  owner names: the owner always places its own keys, so the old holder asks it (`FindRelayHolder`). The new holder takes a carrier only for a key its own view makes it hold, from a node
  that holds relay mail: custody can jump between holders that do not know each other, as when a
  holder gives back the keys of a run a relay-capable node joined.

## Finger-table convergence

### Algorithmic basis and Rings policy

Chord defines finger slot `i` as the successor of `local + 2^i`. The batching
lemma Rings relies on is Chord's own: in the
[SIGCOMM 2001 paper](https://pdos.csail.mit.edu/papers/chord:sigcomm01/chord_sigcomm.pdf),
section 4.4, `init_finger_table` (Figure 6) checks whether `finger[i].node` is
also the correct `finger[i+1]` entry, which holds exactly when no node lies in
`finger[i].interval`; one lookup therefore answers every consecutive slot whose
target is no farther than the returned successor, and the paper derives its
`O(log N)` expected-lookup bound from that check. The same section's advice to
copy a neighbour's finger table as initial hints is the origin of the
inferred-hint model below. The
[ToN 2003 revision](https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf)
dropped both in favour of randomized `fix_fingers`. Rings-specific are only the
convergence state machine around that lemma: UUID correlation, per-slot
evidence epochs, cyclic slot selection, admission leases, exponential retry,
jitter, and browser lifecycle handling.
[Pamela Zave's Chord correctness work](https://arxiv.org/abs/1502.06461)
supports the separation used here: ring correctness depends
on a valid successor structure; fingers are replaceable routing optimization.

Rings treats entries learned from admission, successor changes, and peer removal as routing hints,
not as proof that a finger slot is current. A lookup for slot `i` verifies the consecutive range
from `i` through the highest slot whose target is no farther than the returned successor. This
reduces sparse-table convergence from one lookup per bit to one lookup per distinct successor
range. Lookups are issued for unverified ranges in cyclic order, resuming after the range the
previous attempt proved or failed, so a range whose successor never completes a handshake costs one
attempt per rotation rather than blocking every range above it; periodic revalidation of proved
ranges is deferred by pending work only while that work is succeeding. A topology change invalidates
only slots whose hints changed. A node with no admitted successor keeps its ranges unverified but
dormant: temporary isolation is not proof that the global membership set is empty, and the first
successor admission activates the pending pass. The current successor's authenticated stabilization
report proves the local successor interval only when it echoes the current stabilization UUID and
reports the local node as its predecessor; a superseded or reordered report is ignored, and there is
no token-less path by which a report can change the successor list. A new stabilization round is not
started while the head's previous report is still being processed, and a handler releases its claim
on every exit path, so a slow candidate handshake neither loses the report nor blocks the next round
for longer than the handshake. Successor-list synchronization likewise accepts only one report whose
reporter and UUID match an outstanding query to a current successor; unsolicited, duplicate, and
superseded reports, and reports from a reporter that has left the successor list, are dropped before
they can start connection admission. Both paths revalidate that claim before each candidate: a
report keeps its remaining candidate budget while its reporter stays a successor (its own
admissions extend the list without revoking it), and loses it once the reporter leaves. One
candidate permit already issued before that may still finish its connection and admission; no later
candidate from that report is permitted. Candidate lists are deduplicated and ordered by transport
quality: successor-list sync admits at most the local successor capacity, while stabilization may
additionally admit one predecessor. Those locally proven finger slots never send a lookup around the
ring.

Each node keeps at most one finger operation outstanding, either awaiting a lookup report or retaining
a timely proof while its returned peer completes transport admission. Reports echo a fresh 128-bit
UUID request identifier allocated at the effect boundary. Lookup expiry is checked and the proof is
retained before connection setup is awaited, so the shorter lookup deadline cannot expire during the
longer handshake. The retained proof has its own admission lease of 210 seconds, longer than the
180-second handshake generation that will own it, so the lease is only a backstop for a proof that
never gains a generation; if the handler is cancelled or transport admission never completes, expiry
releases the slot and increases the retry backoff. Results from an expired request or from a request invalidated by a topology change
cannot overwrite newer state, including after a process restart. A fleet's first automatic attempt is spread over a
node-lifecycle-randomized 10-second per-node phase window, so an identity cannot preselect its time
bucket. Repeated `stop`/`listen` cycles on the same browser provider reuse that phase instead of
rerolling it. When browser suspension leaves a runnable finger deadline at least 10 seconds overdue,
the node spreads the stale attempt over a new 1--11 second delay rather than emitting on resume; a
retained admission proof stays dormant until that lease expires, and a full provider reconstruction
is a new lifecycle and selects new entropy. Listener generations on one browser provider are serialized,
so a new `listen` waits for a stopped generation's cooperative cleanup instead of running duplicate
maintenance daemons. An in-flight lookup exposes its remaining timeout to the listener instead of an
absolute timestamp from another clock origin, so repeated listener restarts still wake at the same
physical expiry.
Send or handshake cancellation, invalid reports, lookup timeouts, and admission-lease expiry increase a progress-sensitive
retry floor through 2, 4, 8, 16, 32, and 60 seconds. A topology change invalidates superseded evidence
without classifying normal churn as a network failure;
each retry is additionally spread across a full jitter window of the same size. Only progress
resets that failure level: an applied range proof, or a stabilization report that locally proves the
range the active attempt was verifying. Other missed
deadlines schedule one future attempt rather than catch-up bursts. Finger convergence may yield to
at most two due topology/storage phases before its turn is reserved.

One due node emits one routed lookup rather than a broadcast. Its normal completion is one routed
report. Discovering an unconnected result also runs connection admission, which sends a connect
lookup and may query the new successor's bounded successor list; those peers can in turn require
connections. The sync-storm gate therefore executes the real effect and transport path and counts
the complete causal submission trace instead of multiplying a fixed message-leg estimate. The
per-message relay hop budget bounds each carrier, while the retry schedule bounds only the originating
node's finger emissions.

The finite topology model verifies correlation tokens, retry resource bounds, stale proof rejection,
and single-use connection permits over an exhaustive action alphabet, while the sync-storm fixtures
exercise the full production transport cascade. On the receiver side, per-node convergence traffic
composes with `SlotRoster` (`330` connection slots with guaranteed per-slot floors and fair shared
pools), `NodeFramePool` (`408 MiB` node-wide frame ceiling), `InboundCapacity` (`256 MiB` reassembly
budget), and per-origin rate quotas so synchronized bootstrap bursts stay strictly bounded at every
destination.
