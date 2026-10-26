# Transport flow control

Every connection carries four lanes, each pinned to one ordered data channel, and every lane
is credit flow controlled (#924). Receive-side admission is lossless: no honest frame is refused
for want of credit or mailbox capacity. Overload reaches the sender as backpressure instead. One
hold of core is bounded on its own and does drop: a frame that finds the pre-admission hold full
(it arrived before this end admitted the connection) is dropped, uncharged. Two link-level
verdicts drop a frame too, uncharged and never for want of capacity: a frame whose delegation
reference misses (for an honest sender only past two delivery bounds of transit under a full
table), and a frame whose origin delegation expired in flight (see [Delegation references on a
link](delegation-references.md)).

## Credit

A receiver holds at most `LANE_CREDIT_WINDOW` (16) frames of a lane, so at most
`INBOUND_PEER_FRAME_CAPACITY` (64) frames of at most `MAX_DATA_CHANNEL_MESSAGE_SIZE` bytes per
connection. It advertises more credit, as a cumulative and idempotent `TransportMessage::Credit`
frame, as the protocol takes frames over. A sender holds a custom frame until its lane has
credit. A received credit frame is applied on arrival, never behind received data, and is not
itself charged. On the sending side a credit frame goes out on its own lane, so it may wait
behind up to a window of this end's committed frames on that lane.

Credit counts frames, not bytes, so it caps each lane's throughput toward one neighbour at a
window per round trip: about 160 small messages per second at a 100 ms round trip (53 at
300 ms), or about 10 MiB/s of 64 KiB frames (3.3 MiB/s). Byte-denominated credit, sent ahead of
queued data, would lift both (#937).

The laws, model checked with stateright over every interleaving, with credits reordered and
duplicated (`rings-transport`, `core::credit`):

- an honest sender never exceeds the advertised credit;
- a lane's occupancy never exceeds its window;
- an honest sender is never blocked forever while the receiver keeps consuming, makes every
  advertisement it deferred, and resends every credit whose send failed;
- one lane's backlog never blocks another lane.

A frame beyond the advertised credit is a protocol violation: it is refused, reported to the
callback as an invalid frame, and logged.

What a node holds of received frames is bounded twice:

- **Hard, per connection.** A connection holds at most 4 lanes × 16 frames × 64 KiB = 4 MiB at
  the receiver under credit. Connection admission keeps at most `2 × (160 + successors + 1)`
  connections (328 with the default 3 successors), so a node holds at most about 1.3 GiB, and
  that only if every connection fills the credit it holds at once.
- **Soft, per node.** While the frames a node's connections hold together exceed 16 MiB
  (`NODE_RECEIVE_SOFT_LIMIT_BYTES`), no lane but the priority lane advertises new credit; a
  lane that releases a batch meanwhile defers its advertisement, and the release that brings
  the node below the limit makes every deferred one. The priority lane
  (`ChannelLane::PRIORITY`), which core gives to DHT control, is never deferred. The receiver
  refuses as invalid a frame whose content belongs to another lane than the one it arrived on
  (a chunked message by the lane every chunk shared), so the priority lane's credit carries
  DHT control alone. A peer's control traffic, and with it every link's liveness, therefore
  never waits for what other peers make the node hold. Deferring narrows what senders may send
  next, never what they were granted, so no honest frame is refused; the node exceeds the
  limit by at most the credit already advertised and one priority window per connection. A
  hard node-wide bound needs credit that can be taken back, a wire change (#934).

A wait for credit is backpressure, not a verdict on the peer: it ends when credit arrives or
when the connection generation ends (it fails or closes), and the wait itself never retires a
link. A receiver may legitimately consume slowly (its inbound mailbox is busy with other
peers' work), and whether a peer is alive is liveness's to judge. Liveness reads no credit state; it
judges every admitted peer by probes alone (`swarm::transport::liveness`):

- every `PEER_LIVENESS_PERIOD_MS` (15 seconds) after its previous probe, a peer is due a probe,
  whatever its traffic or credit, and at most one probe per peer is outstanding;
- a probe's deadline, `PEER_LIVENESS_TIMEOUT_MS` (45 seconds), counts from its issue, so a probe
  the peer will not let this end send, for want of the control lane's credit, waits in the
  peer's own queue and goes unanswered;
- only the answer to the outstanding probe clears it: the peer's own traffic proves it can
  send, not that it takes this end's control traffic, so a peer that withholds credit cannot
  escape eviction by sending;
- the probe rides the control lane, so a peer slow only on a data lane answers it and is kept;
- a probe takes no shared outbound capacity (one outstanding per peer bounds it), so other
  transfers waiting on the peer cannot refuse it, and no probe waits on its peer, so one starved
  peer delays no other peer's probe. Its transfer waits for credit until the probe's own answer
  deadline, not the 25-second first-frame deadline of other protocol sends;
- a probe that could not reach the queue (no ready connection, or a step cut short) stays
  outstanding, with its deadline still counted from its first issue, and is sent again on the
  next pass, never withdrawn, so a peer gains nothing by keeping its connection unready;
- a probe's answer takes no shared capacity either while its next hop holds no other answer in
  flight (one per next hop); a further answer to the same hop falls back to the shared
  capacity, as any protocol send.

The outbound worker holds a lane's credit only while a transfer of that lane awaits it, so no
credit lingers at the sender after its transfer went away. A peer that does not progress this
end's control traffic is therefore evicted within the period plus the deadline, and its
eviction fails every wait on its credit. A composed model (`liveness::test_composed_model`)
checks these laws over credit, lanes, the application inbox and a third peer, and refutes each
design they replace.

A wait that pins one of the sender's budgets is bounded by what that budget is for: a
link-control frame, which holds a link-control permit, waits at most one delivery bound, and
is then dropped, since the next frame that misses or teaches its delegation repeats it; a
scheduled transfer is bounded by its own send deadline.

A remote-created data channel is admitted only by the lane its label names, and only once per
lane.

## Backpressure

- The core inbound mailbox makes an arrival wait rather than refuse it, while the arrival keeps
  its credit. Waiting arrivals are admitted in resource order
  (`fair_admission::ResourceOrderedQueue`): first come first served per exhausted resource (a
  peer's budget, a lane's share, the shared pool) and per peer, lane and transport carrier. A
  frame within its lane's reservation, such as DHT control, therefore never waits behind
  another lane's borrower, and one peer at its budget never holds back another peer. The order
  is model checked: no overtaking on an exhausted resource, stream order, no admissible arrival
  left waiting, and every wait resolved.
- A chunked message is charged once, at its first chunk `[0, n]` of `d₀` bytes, for the
  `n·d₀` its buffer and its completed copy take (#932; load-bounds paper, Law 4). That charge
  waits as an arrival like any frame's; the message's later chunks take nothing, and they wait
  only for their own carrier's earlier frames, so they never wait behind capacity their own
  message holds. The completed message carries the charge into dispatch: no reassembled
  message is ever refused or dropped for capacity, and an expired one releases its charge.
- Core's outbound scheduler serves only lanes that hold credit, and times a send only for the
  transport's acceptance, never for the receiver's consumption.
- An inbound lane's progress depends only on local resources (`swarm::transport::egress`).
  Every send made in the protocol context (a handler's forward, report, query or
  notification, a connection offer it starts, a read repair, a relay hold, a liveness probe
  or its answer, and maintenance's queries) goes through a protocol egress, the only senders
  handlers and maintenance can reach; the waiting discipline is constructible only at the
  Swarm API. A protocol send takes the connection's readiness and its outbound capacity now
  (lacking either, it is dropped as local backpressure), returns once queued, and so frees
  the inbound event and lane that carried its cause. The next peer's backpressure never
  reaches upstream links, and neither does a liveness verdict.
- A connection a handler starts or answers is reserved and created at once, and its offer or
  answer, whose ICE gathering waits on STUN or TURN, is negotiated and sent apart from the
  event.
- A protocol payload's first frame's admission, credit included, is awaited apart from the
  event, at most the first-frame deadline (25 seconds; a liveness probe, its own answer
  deadline), and a payload that waits longer is dropped, as a best-effort relay may drop; the
  origin's own tracked send or retry covers it. A send through the Swarm API, the application
  boundary, waits for its first frame's admission, so its caller learns whether it left.

## Application inbox

Core never calls, awaits or waits on application code (#935). The application reads its data
and peer facts from one bounded inbox (`ApplicationInbox::next`, or `try_next` without
waiting) at its own pace, and no inbound lane ever waits for it to read:

- Application delivery is lossy by design. A live application message that does not fit the
  full inbox is dropped, counted (`ApplicationInbox::dropped`) and logged at `warn`, as a
  best-effort network drops it, and nothing tells its sender. A drop of a message from an
  admitted peer is told to the application itself, as `SwarmEvent::MessageDropped` (one per
  peer while it is unread), so a protocol whose state rides that link, such as an onion
  session, can end that state instead of losing a cell silently (#969). A protocol that needs a
  reliable stream detects loss end to end: the TCP relay numbers its segments and resets a
  stream whose gap outlives `REORDER_DEADLINE` (four delivery bounds: a superseded path with
  at most four hops left to cross) or the replay window, closing the local socket
  abortively, and the E2E stream decryptor fails a stream that made no progress for its stall
  deadline (#939). The bound,
  `APPLICATION_INBOX_WEIGHT` (64 MiB), weighs each message by its own size plus the byte
  length of its data, one rule for every kind, so empty messages are bounded too.
- Before the application claims the inbox, live messages are discarded and peer facts are
  kept, so a late reader still learns the peers already admitted. Once the inbox is dropped,
  everything is discarded.
- Messages and peer facts share one queue, in the order core produced them. An unread
  `PeerAdmitted` is withdrawn by its `PeerRetired`, together with the drops after it, and a
  drop is told once while one waits, so a peer has at most four facts queued
  (`[MessageDropped] [PeerRetired] [PeerAdmitted] [MessageDropped]`).
- Mail stored for this node in its relay inbox is offered only through a reservation of room
  in the inbox, and retired from storage once the application has taken it from the inbox.
  Taking ends core's custody: what the application does with the message afterwards is its
  own (the node's `Backend`, below, may drop it beyond its namespace queues). Mail that finds
  no room or no reader, or whose origin's quota is spent for now, stays stored for a later
  pass; an origin with a spent quota does not hold back the others. Delivery is at least once
  within a run: a restart between the taking and the retirement loses the mail, as the replay
  window refuses its redelivery (#936).

The node's `Backend` reads the inbox and dispatches each registered namespace on its own
queue, bounded by 256 envelopes and by 16 MiB of encoded envelopes (an empty queue admits any
one envelope), in arrival order, so one slow protocol holds back neither another namespace nor
the peer facts. An envelope that cannot be decoded, names no registered namespace, finds its
queue full or its namespace unserved is dropped, logged at `warn` and counted by reason
(`Provider::dispatch_drop_counters`, FFI `dispatchDropCounters`). Protocols are registered
before the inbox is claimed: from the claim on, an envelope of a namespace not yet registered
is dropped. Facts and E2E messages reach the `BackendObserver` at once, so they may overtake
custom messages still queued for a namespace. The FFI provider's E2E event buffer is likewise
bounded (4096 events) and drops further events, counted (`takeE2eEvents` reports `dropped`).

Every such wait is logged at `warn`, with the peer and lane: a sender waiting for credit, and an
arrival waiting for the mailbox.
