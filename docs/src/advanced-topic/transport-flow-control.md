# Transport flow control

Every connection carries four lanes, each pinned to one ordered data channel, and every lane
is credit flow controlled (#924). Receive-side admission is lossless: no honest frame is refused
for want of credit or mailbox capacity. Overload reaches the sender as backpressure instead. Two
holds of core are bounded on their own and do drop: a frame that finds the session-link hold full
(it waits for a delegation the peer has not backed; the holds of every link together keep at most
256 frames) or the pre-admission hold full (it arrived before this end admitted the connection)
is dropped, uncharged.

## Credit

A receiver holds at most `LANE_CREDIT_WINDOW` (16) frames of a lane, so at most
`INBOUND_PEER_FRAME_CAPACITY` (64) frames of at most `MAX_DATA_CHANNEL_MESSAGE_SIZE` bytes per
connection. It advertises more credit, as a cumulative and idempotent `TransportMessage::Credit`
frame, as the protocol takes frames over. A sender holds a custom frame until its lane has
credit. Credit frames are applied on arrival, never behind queued data, and are not themselves
charged.

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
  that only if every connection fills the credit it holds at once. Session-link holds add at
  most 256 frames (16 MiB) across the node.
- **Soft, per node.** While the frames a node's connections hold together exceed 16 MiB
  (`NODE_RECEIVE_SOFT_LIMIT_BYTES`), no lane but the priority lane advertises new credit; a
  lane that releases a batch meanwhile defers its advertisement, and the release that brings
  the node below the limit makes every deferred one. The priority lane
  (`ChannelLane::PRIORITY`), which core gives to DHT control, is never deferred, so a peer's
  control traffic, and with it every link's liveness, never waits for what other peers make
  the node hold. Deferring narrows what senders may send next, never what they were granted,
  so no honest frame is refused; the node exceeds the limit by at most the credit already
  advertised and one priority window per connection. A hard node-wide bound needs credit that
  can be taken back, a wire change (#934).

A wait for credit is backpressure, not a verdict on the peer: it ends when credit arrives or
when the connection generation ends (it fails or closes), and the wait itself never retires a
link. A receiver may legitimately consume slowly (its application reads its inbox at its own
pace), and whether a peer is alive is liveness's to judge. Liveness reads no credit state; it
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
  peer delays no other peer's probe.

The outbound worker holds a lane's credit only while a transfer of that lane awaits it, so no
credit lingers at the sender after its transfer went away. A peer that does not progress this
end's control traffic is therefore evicted within the period plus the deadline, and its eviction fails every wait on its credit. A composed model
(`liveness::test_composed_model`) checks these laws over credit, lanes, the application inbox
and a third peer, and refutes each design they replace. A wait that
pins one of the sender's budgets is bounded by what that budget is for: a link-control frame,
which holds a link-control permit, waits at most the session hold, after which the frame it
would answer is gone, and is then dropped; a scheduled transfer is bounded by its own send
deadline.

A remote-created data channel is admitted only by the lane its label names, and only once per
lane.

## Backpressure

- The core inbound mailbox makes an arrival wait rather than refuse it, while the arrival keeps
  its credit. Waiting arrivals are admitted in resource order
  (`fair_admission::ResourceOrderedQueue`): first come first served per exhausted resource (a
  peer's budget, a lane's share, the shared pool) and per peer and lane. A frame within its
  lane's reservation, such as DHT control, therefore never waits behind another lane's
  borrower, and one peer at its budget never holds back another peer. The order is model
  checked: no overtaking on an exhausted resource, stream order, no admissible arrival left
  waiting, and every wait resolved. The order covers arrivals; a reassembled message is
  re-charged at its full size on the actor, where it cannot wait, and is dropped if the
  budgets cannot hold it (#932).
- Core's outbound scheduler serves only lanes that hold credit, and times a send only for the
  transport's acceptance, never for the receiver's consumption.
- An inbound lane's progress depends only on local resources (`swarm::transport::egress`).
  Every send made in the protocol context (a handler's forward, report, query or
  notification, a connection offer it starts, a read repair, a relay hold, a liveness probe)
  goes through the protocol egress: it takes the connection's readiness and its outbound
  capacity now (refused for either, it is dropped as local backpressure), returns once queued,
  and so frees the inbound event and lane that carried its cause. The next peer's
  backpressure never reaches upstream links, and neither does a liveness verdict. Its first
  frame's admission, credit included, is awaited apart from it, at most
  `DETACHED_FIRST_FRAME_TIMEOUT` (25 seconds), and a payload that waits longer is dropped, as
  a best-effort relay may drop; the origin's own tracked send or retry covers it. A send
  through the Swarm API, the application boundary, waits for its first frame's admission, so
  its caller learns whether it left. No application code runs on a lane: the application
  reads its data and peer facts from a bounded inbox at its own pace (#935), and a full inbox
  holds only the application data lanes (`Application`, `E2e`).

Every such wait is logged at `warn`, with the peer and lane: a sender waiting for credit, and an
arrival waiting for the mailbox.
