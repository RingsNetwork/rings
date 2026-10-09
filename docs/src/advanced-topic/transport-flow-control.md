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
  (`NODE_RECEIVE_SOFT_LIMIT_BYTES`), no lane advertises new credit; a lane that releases a
  batch meanwhile defers its advertisement, and the release that brings the node below the
  limit makes every deferred one. Deferring narrows what senders may send next, never what
  they were granted, so no honest frame is refused; the node exceeds the limit by at most the
  credit already advertised. A hard node-wide bound needs credit that can be taken back, a
  wire change (#934).

A wait for credit is backpressure, not a verdict on the peer: it ends when credit arrives or
when the connection generation ends (it fails or closes), and the wait itself never retires a
link. A receiver may legitimately consume slowly (a protocol handler may take up to 30 seconds),
and whether a peer is alive is liveness's to judge. Liveness does judge a peer that withholds
credit:

- a liveness probe counts as sent once it is queued on the peer's control lane, so a probe the
  peer will not let this end send, for want of the control lane's credit, is unanswered, and
  the peer is evicted once the answer window passes;
- a peer this end has waited `PEER_LIVENESS_IDLE_MS` (15 seconds) for credit on any lane is due
  a probe however recently it sent anything; the probe rides the control lane, so a peer slow
  only on a data lane answers it and is kept;
- probes are sent concurrently, each returning once queued, so one starved peer delays no other
  peer's probe.

A peer that does not progress this end's control traffic is therefore evicted within the idle
interval plus the answer window, and its eviction fails every wait on its credit. A wait that
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
  transport's acceptance, never for the receiver's consumption. A payload the node forwards or
  relays is released once it is queued: it takes its outbound capacity without waiting (a
  forward refused for capacity is dropped as local backpressure), returns, and frees the
  inbound event and lane that carried it, so the next peer's backpressure never reaches
  upstream links, and neither does a liveness verdict. Its first frame's admission, credit
  included, is awaited apart from it, at most `DETACHED_FIRST_FRAME_TIMEOUT` (25 seconds), and
  a forward that waits longer is dropped, as a best-effort relay may drop; the origin's own
  tracked send or retry covers it. A payload the node originates returns once its first frame
  is admitted, so its sender learns whether it left.

Every such wait is logged at `warn`, with the peer and lane: a sender waiting for credit, and an
arrival waiting for the mailbox.
