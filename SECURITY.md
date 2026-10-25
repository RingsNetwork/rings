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
message; anything that needs an onion loop belongs to the privacy layer.

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
successor answer and forwards it only over a direct link. The hop budget is unsigned: it
bounds honest work, not a dishonest hop. A leak here is a communication-layer bug; loops
inherit whatever the relay exposes.

The carrier records no hop history, with one exception (#947): connect signaling (an offer,
its answer, and a tie-break counter-offer) carries the relays it crossed, so the reply
retraces them, each hop a direct link of the one before. Every hop and both ends learn that
path; its privacy is already spent, since the two ends exchange addresses in their SDP.
Inbound validation refuses a path on any other message and a path that names a node twice.

### Privacy layer

`crates/node/src/onion` carries client-sealed loops of fixed-width Sphinx cells over direct
edges (#843).

- **Per-hop knowledge.** Every edge of a loop carries one cell `α‖β‖γ‖y` of exactly `b`
  bytes. Each position peels one Sphinx layer, which names its successor, its application and
  the loop's expiry, and removes one AEZ layer of the carry `y`; a relay keeps no state for
  the loop. Only the symbol hop opens the client's value, and only the client knows the whole
  loop and recognises its return by the tag `γ` it drew. Replies carry no exit signature:
  they open under a key only the client and the exit's process hold, so they are deniable.
- **Route shape.** A route is a loop `g → r → exit → r' → g` through the client's entry
  guard `g`, with `H = 3n + 2` positions for `n ≤ 4` symbol hops (`H ≤ 14`), every position
  but the guard's return a distinct node. Selection fails closed rather than shorten a route,
  since a short path is a distinguishable segment length. The forward segment carries the
  client's value to the exit; the return segment carries the reply, from a reply block the
  client sealed, back through `g`.
- **Exit services.** An exit descriptor signs one service of the closed signature
  `Σ_W = {tcp, https}`; `relay` is a capability, not a service, and any other name is
  rejected wherever it enters a node. `https` is a fetch only; every byte tunnel is `tcp`,
  and no interpretation falls back to another symbol.
- **Admission and replay.** A hop charges `u(b)` units of the sender's budget per window
  `V` on receipt, before it decodes `α` or computes an ECDH; only a charged cell can be
  peeled. One process-local store keyed by `(expiry, ν)` admits a layer at most once, and
  each node draws a fresh process epoch per start, which invalidates cells built before it.
- **Emission** (#880). While a link is up it emits one cell per slot at one of two constant
  rates and never above them: the active rate `r` (one 16 KiB cell per ~10 ms) and the idle
  floor `r_idle` (one unit per second; a node may lower it, never raise it). A queued real
  cell takes the next slot and every other slot carries cover, so real cells replace cover,
  never add to it. A link stays active for at least `D = 30 s` after a real cell. A slot is
  emitted only while the link's Application transport lane has credit (#913), read through a
  read-only view, so the rate is `min(r_phase, the receiver's release rate)`,
  `r_phase ∈ {r, r_idle}`: the floor and the dwell hold while the receiver releases, and a
  credit wait delays real and cover cells alike. Cells use fixed size classes from 16 KiB to
  12 MiB, preserved across every edge.
- **Session fate.** A session direction ends with `fin(n)` only when its bytes really ended,
  and with `abort(n)`, carrying no reason, on any failure. A receiver reorders within a
  bounded window; a sequence gap that persists past it, a frame that finds the session's queue
  full, a loop the exit cannot place, or a link retired or overloaded under the session's
  loops at an endpoint fails the session closed at that endpoint, which tells the other with
  `abort`; an exit holding no reply block answers the session's next loop with `abort`. Relays
  hold no session state, so a loss anywhere on the loop is observed by the endpoints as a gap
  (not yet complete: a session with no reply in flight sees no gap, so the death of a link that
  is neither endpoint's own is not yet observed at both endpoints, #975), and no truncated stream is presented as complete: a client resets its local stream (an RST,
  not a FIN; a captured gateway flow is reset through its connector), a refused CONNECT is
  answered `502`, and an `https` fetch fails. Accepted: once a download has ended with `fin`, a
  later failure of the upload closes the local stream cleanly, so the local application is not
  told that the exit could not write its last bytes.
- **Entry guards.** A client pins a persisted set of first hops, limiting how many relays
  see its edge over time. The guard store is a durable cache (#911): a completed guard write
  survives a process or system crash, as a replay write does, so a crash never rotates the
  first hop. A guard record that is absent or does not decode (only a loss outside the store or
  a schema change can produce one) is retired, logged at `warn`, and re-selected: losing the
  set costs one re-selection of the first hop, never onion routing. A browser whose IndexedDB
  cannot open keeps its guards in memory, re-selected on every start.

A loop hides hops from each other and the client from the exit. It does not hide the client
from its first hop, overlay membership, registry lookups, the loop's round-trip time from the
guard, its class and expiry quantum from every hop, or each link's active/idle phase and its
receiver's release rate (its load, and through the work a real cell costs it, a smeared trace of
the real fraction) from an observer of that link. Anonymity also depends on the candidate set: registry descriptors are
signed and expire, but a party with many identities can try to hold several positions of a
route.

## Feature Boundaries

- **Replay.** The destination keeps a persisted 32-sequence window per
  `(network_id, origin, destination, traffic class)` stream, so a transaction is dispatched
  at most once while the store is retained; a damaged record disables only its stream.
  A completed replay write survives a process or system crash: the native store flushes the
  file and its directory (on unix), and the browser store commits each IndexedDB transaction
  with `durability: "strict"` (#912). Exactly-once effects are not claimed. In a browser the
  replay guarantee lasts as long as the browser keeps the origin's storage: a private window
  or ephemeral partitioned storage discards it when the session ends, which resets every
  stream as an operator's removal of the replay store does, and a page cannot detect such a
  context (#1036).
  [Details](docs/src/advanced-topic/transaction-replay.md).
- **Rate limits.** After verification, the destination charges per-origin message and byte
  buckets keyed by the account DID, committed together with replay and before the application
  sees the message. Rotating keys or relays does not reset them, and a drop never disconnects
  the relay.
- **Application boundary.** Core never calls, awaits or consults application code (#935): the
  application cannot see, validate or veto a protocol message, nor send one, and no inbound
  lane waits for it. A slow application therefore costs only its own messages, never a peer's
  credit or another link's liveness. Application delivery is lossy by design: a message that
  does not fit the full inbox is dropped and counted, unknown to its sender, so a protocol
  that needs a reliable stream detects loss end to end: the TCP relay resets a stream with a
  segment it cannot recover, and an E2E stream with a lost frame fails as stalled, so neither
  passes on a stream with a silent gap (#939). Relay mail leaves core's
  custody once the application takes it from the inbox. It is delivered at least once across
  restarts: a delivery admitted and not yet taken is kept in relay custody inside its replay
  record, so a restart redelivers it instead of refusing it as a replay (#936). A crash after
  the take and before its release delivers it once more, with the same transaction id, which an
  application that needs exactly-once delivery deduplicates by. The carrier itself sits in a
  retained relay store, so a crash or the data store's budget cannot drop the mail (#1021).
  [Details](docs/src/advanced-topic/transport-flow-control.md#application-inbox).
- **Rerouting.** A remote placement of a Swarm storage operation refused before backend
  acceptance is retried from topology recomputed after a topology, link or capacity event,
  never after a duration, within a budget; it is applied at most once (#859). A
  protocol-context write makes one attempt and never waits on a peer.
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
  while the carrier is retained. A relay inbox is verified by its holder, readable only by
  its recipient, and removable only by its recipient or, in an ownership hand-off, by the
  previous holder once the next one acknowledged holding exactly that value; it is capped at
  64 messages. Relay carriers live in a store of their own (#1021), retained: a completed hold
  survives a crash, and the data store's budget never evicts held mail; a full relay store
  refuses a new hold instead. A node without a retained relay store (a browser whose IndexedDB
  cannot open, a library node built without one) declares in its handshake that it holds no relay
  mail, and relay custody moves past it: an inbox is held by the first node at or after its key's
  owner that holds relay mail, which every relay path names by one function (#1037). A hold no node
  within the writer's successor list can take is refused to the writer, never dropped. The holder of
  a key behind its predecessor admits a hold only from a storage-less node of the run at or after
  the destination, and the previous holder it learns is passed from predecessor to successor, so a
  dishonest node of a run can at most make the holder after it refuse or misplace mail whose key
  lies on that run. A writer keeps every hold until the holder it sent it to acknowledges it, and
  sends it again on later repair passes until a holder takes it or its sender proof expires, so a
  lost acknowledgement or refusal costs a resend, never the mail; only the node a hold was sent to
  can acknowledge or refuse it, and at most 64 holds are kept, another being refused to its local
  sender. A holder takes a relocated carrier only for a key its own view makes it hold, from a node
  that holds relay mail, and only elements that pass the witness; a keeper that cannot place a
  carrier's key asks the key's owner for its holder, and a wrong answer moves nothing, as the
  named node takes only a key its own view makes it hold. Values are stored in the clear.
  Browser nodes hold relay mail too, but a browser can only claim that its storage outlives the
  session, and a closed tab strands what it holds, so held mail is protected by redundancy
  instead of by one holder's durability (#1036): every carrier is replicated down a chain to
  the first r nodes after its key's owner that hold relay mail, 8 by default, and the writer is
  acknowledged only once all of them hold it (#777). Held mail is lost only if every member
  holding it is lost within one repair delay: seconds after a departure the members see, and up
  to the 45 s liveness timeout after a silent one. A node takes custody only after 2 minutes
  online, so members are the nodes that stay. Members trust each other's copies as holders do;
  this replication guards against churn, not against a dishonest member.
  [Details](docs/src/advanced-topic/chord.md#data-topic-storage),
  [relay inbox](docs/src/advanced-topic/chord.md#relay-inbox).
- **Transport flow control.** Every lane of a connection is credit flow controlled: a receiver
  refuses no honest frame, refuses and reports a frame beyond its credit, and pushes overload
  back to the sender. One peer's backpressure never holds another's control lane (lane 0, which
  the node-wide receive bound never defers; that bound has no per-peer share, so one peer's
  load can defer the other lanes of every link). Liveness reads no credit state, so a peer
  that withholds the control lane's credit is evicted whatever else it sends. The node-wide
  receive bound is soft (#934).
  [Details](docs/src/advanced-topic/transport-flow-control.md).
- **Delegation references.** A link references a delegation only under a bounded lease its
  receiver granted to a frame that verified, sent as a remaining duration, so no two clocks
  are compared. The receiver pins at most 128 delegations per link, 8 of them reserved for
  the link peer's own, and refuses a new lease while every slot is still in its grace; the
  sender then sends inline. A frame
  whose reference misses is dropped uncharged, never held, so a lane's frames reach admission
  in arrival order (#908). An origin delegation that expired in flight is not charged to the
  hop that carried it, and only when the rest of the frame verifies. An unrelated party
  cannot populate a link's tables. [Details](docs/src/advanced-topic/delegation-references.md).
- **Verified delegations.** A delegator's signature over a delegation is recovered once per exact
  delegation and memoized under the full Keccak-256 of its encoding, so a delegation differing in
  any byte, its signature included, is checked anew (#1089). The memo serves a delegation only
  before its expiry minus one delivery bound, the expiry is checked on every message before the
  memo is read, and the memo holds at most 329 delegations; eviction costs a recovery, never a
  verdict. The delegatee's signature on each transaction is always checked. A delegation has no
  revocation path; it ends at its expiry.
- **Wire decoding.** Every decoder that admits relayed bytes has generated malformed-input
  tests in its crate.
- **Native gateway.** A TUN gateway starts only on `enabled: true` or `--gateway`, captures
  nothing until the operator lists a prefix, and never grants itself host capabilities.
- **DNS relay.** A relay serves only what an owner signed: an owner's name answers from its
  newest descriptor that verifies under the owner's DID, so no node can publish records or
  services for another (#1049). The ingress carries a connection only to an address its DNS
  handed out and a port the owner announced, and the owner's node dials only the
  `dns-relay/<port>` services its own configuration registered, so a relay is no open proxy
  into a node. The address map is keyed by the relay's secret, so no one can grind a DID onto
  another's address; a colliding second owner gets no address. Admission bounds the
  connections carried at once, in total and per owner. The relay sees what it carries: the
  bytes cross the overlay in the TCP relay, readable by the relay and by every hop, so a service
  that needs confidentiality ends TLS on the node (a certificate for `<did>.<zone>` by ACME
  DNS-01 through the owner's own TXT records). The relay keeps answering for a vanished owner
  for its grace period, from the last descriptor it verified; an owner withdraws at once by
  announcing nothing. A served port is a service of the node's TCP relay, so any overlay peer
  that opens `dns-relay/<port>` reaches its target, not only relays: map only what may face the
  internet. The PROXY protocol header of a `proxy_protocol: v2` port is unauthenticated, as the
  relay or any such peer writes the first bytes; a service may log, rate or filter by the client
  it names, never authorize by it. The relay bounds what strangers make it do: an `ANY` query
  gets one record set (RFC 8482), at most `max_lookups` DHT lookups run at once (the rest are
  answered from memory or refused), at most `max_owners` owners are kept, and the ingress takes
  a connection's slot before routing it and IPv4 connections never reach it.

## Non-Goals

- Sybil and eclipse resistance. DIDs stay key digests for Ethereum and Solana addressing, so
  the planned defence removes position choice instead of pricing identities:
  `pos(did, e) = H(did, beacon(e))` (#780).
- Unlinkability on the communication layer.
- Resistance to a global traffic observer.
- Availability against Byzantine storage owners or route candidates.

## Known Limitations

- #918: a registry descriptor that another node re-appends after its registrant stopped stays
  listed until its signed lease ends, at most 10 minutes; without a replay, a stopped
  registrant leaves the listing within its TTL.
- #920: a node's store of uncollected removals refuses past 1024 per sender and 65536 in all;
  filling the total takes 64 senders' identities (the Sybil residual of #907) or a node its view
  makes an owner of the topic. A node whose browser cannot open IndexedDB keeps removals in
  memory, so each restart makes it a fresh replica.
- #921: a data topic holds at most 1024 uncollected tombstones and refuses past them, so any
  writer, or a sync peer, can fill one topic's removal capacity at an owner and refuse further
  removals of that topic there until those tombstones are collected, at most 100 minutes and
  twice the clock-skew tolerance later; a data topic has no write authority, and a forged
  removal received by sync can likewise hold a topic live for up to the horizon past the
  receiver's clock.
- #932: a reassembled message whose full-size mailbox charge does not fit is dropped.
- #1057, #1070: a departure the members do not see (a crash, a dropped network, a suspended or
  closed tab) is repaired only after the 45 s liveness timeout, so r = 8 meets its loss target
  for browser sessions of a minute's median, not of 15 s, until a closing tab leaves gracefully
  (#1057) and chain neighbours probe each other on a shorter timeout (#1070).
- #1036: in a browser context with ephemeral storage, the replay windows reset at every
  session end, undetectably: a receiver may admit an unexpired transaction once more per
  session, and a sender's streams restart at sequence 0.
- #1055: one origin runs one browser node at a time. A native store locks its directory, so a
  second daemon or FFI provider over it fails to open (#1039), but two browser tabs of one origin
  open the same IndexedDB stores and would interleave their writes to the replay and relay
  stores; nothing refuses the second tab yet.
