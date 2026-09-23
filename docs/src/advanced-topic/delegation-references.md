# Delegation references on a link

Every `MessagePayload` carries two proofs: the origin's, over the transaction, and the current
hop's, over the carrier. Both are verified at every hop, not only at the final destination.
Each proof is a delegatee-key signature plus the `Delegation` that delegates that key to an account:
about 150 bytes of delegation next to a 65-byte signature. The delegations are the same for the
lifetime of a connection, so since 0.28.0 a link sends each of them once and references it
afterwards.

## Wire form

A payload frame is the `MessagePayload` with each delegation slot replaced by a reference:

```text
DelegationRef    = Inline(Delegation) | Digest(DelegationDigest)
DelegationDigest = the trailing 20 bytes of keccak256(encode(Delegation))
```

The digest is a content address of the whole delegation
`(delegatee_did, delegator, ts_ms, ttl_ms, delegator_signature)`, not the delegatee key's address. A delegatee key can
carry more than one delegation: an account can re-authorize a new lifetime for it, and any
account can sign a delegation for a key it does not hold, because a delegation needs no proof of
possession. `delegatee_did` therefore does not determine which delegator a message is attributed to;
the content address does. A resolved frame is exactly the payload that would have travelled
inline, so verification, origin attribution, and the transaction digest used for replay and
fork evidence do not depend on how a slot was encoded.

Neither signature covers the delegation slot (both sign the transaction hash), so a forwarding hop
re-encodes the origin slot for its own next link without touching the origin's signature.

## Scope: one link, one direction

A link is one admitted connection generation between two nodes. Each direction keeps one bounded
table, least recently referenced first out:

| End | Table | Capacity | Populated by |
| --- | --- | --- | --- |
| sender | delegations it sent inline, and which of them the peer confirmed | 64 | its own frames; confirmations from the peer |
| receiver | delegations this link carried inline | 128 | frames that verified, and solicited announcements whose delegation verified |

The receiver keeps twice as many as the sender under the same least-recently-referenced order
over the frames both ends saw: the sender touches a delegation on every frame it encodes, the
receiver on every frame it resolved or verified. On a lossless link the sender therefore stops
referencing a delegation (and sends it inline again) before the receiver could have forgotten it.
A frame lost on the link touches the sender's order and not the receiver's, so under loss the
two drift and the receiver may evict a delegation the sender still references; that miss is
answered from the sender's table, which still holds it, at the cost of one round trip and no
charge. A miss the sender cannot answer needs something outside the tables: the two ends
disagreeing on expiry, or a peer that does not follow the protocol.

Consequences of the scope:

- Only the peer at the other end of a connection can populate a receiver table, and only with
  its own bounded share. Nothing an unrelated party says is cached, and an announcement nobody
  asked for is ignored.
- A relay keeps the origin delegations of the traffic it forwards in the table of each outgoing
  link. A destination that never met the origin gets the origin's delegation inline from its last
  hop until it confirms it. No node ever asks the origin for anything.
- Both tables are scoped to the connection generation: the receiver's lives in the callback
  of one connection, the sender's is emptied by the first frame of a newer generation (and
  survives a replacement of the outbound worker under an unchanged generation). A restarted
  node and its peer start from empty tables together; there is no state to resynchronise, and
  a control frame is emitted only on the generation it was judged on, so a late one never
  confirms or disclaims on a generation whose tables never saw it.
- Only frames on the link teach the link's table, and what a verified frame teaches is applied
  (confirmed to the peer, held frames released) before the frame itself is gated for
  admission. A frame from any other peer, or on a callback bound to no handshake, is judged
  self-contained: a reference in it is refused, and it teaches nothing.

## The link is a datagram link

Nothing assumes that frames arrive in order, or at all. WebRTC data channels happen to be
reliable and ordered; the protocol does not use that, so a transport built on QUIC datagrams or
plain UDP would carry it unchanged.

The sender switches a delegation from inline to reference only after the receiver has confirmed it:

```text
sender                                      receiver
  frame, delegation s inline        ───────▶   verifies; learns s
                                 ◀───────   Known(digest(s))
  frame, Digest(s)               ───────▶   resolves from its table
```

- Until the confirmation arrives, every frame carries `s` inline, and the receiver confirms
  every inline arrival that verified. A lost inline frame, a lost confirmation, or any
  reordering costs inline frames, never a stall and never a miss: the reference is sent after
  the confirmation, which was sent after the delegation was learned.
- A reference therefore misses only when the receiver has *forgotten* the delegation (capacity
  eviction, expiry). That is repaired on the link.

## Miss

```text
receiver                                   sender
  frame references d, d unknown
  hold the frame; Request(d)   ───────▶   look d up in the sender table
                               ◀───────   Announce(delegation)  or  Unknown(d)
  announce: delegation verifies → release the frames that now resolve
  unknown / refused delegation  → fail the frames that await d
```

- Held frames are independent of everything that resolves: a frame that resolves is never
  queued behind one that does not, and a held frame never waits for one held before it,
  because the link promises no order to preserve. Among the held frames that resolve at one
  instant, when a later frame or an announcement teaches the delegation they await, the earliest
  arrival leaves first; nothing downstream may rely on more than this.
- The hold keeps at most 16 frames per connection (half the pre-admission hold, so the two
  holds together leave a quarter of the transport's per-peer frames for the control frames that
  release them), each for at most twice the transport's delivery timeout plus one period of the
  inbound actor's periodic cleanup, which drops what waited longer, or whose proof lapsed: a
  frame a peer never backs occupies this end for a bounded time whatever lifetime its proof
  claims.
- Every held frame asks for its own missing digests once, on arrival, so one lost question is
  repaired by the next frame that misses the same digest. A frame that finds the hold full is
  dropped, and the oldest held frame's question is asked again. The sender answers each
  question with exactly one frame.
- A frame the peer was asked about and did not back is charged to the peer as a receive
  failure, as a frame that fails verification is: the peer disclaimed the delegation, announced a
  delegation that does not verify, let the frame wait past the hold timeout, or the frame
  failed on release. A frame that finds the hold full is a loss at this end's capacity, like a
  frame the pre-admission hold cannot take, and is not charged: the peer has not yet had its
  round trip to answer. Nor is a frame whose question this end never managed to send (the
  budget below was spent, or the generation ended): it is swept uncharged, since the peer was
  never asked. A frame released after its connection generation was superseded is dropped,
  never delivered.

Link-control frames (`Known`, `Request`, `Announce`, `Unknown`) are unsigned and idempotent.
They are meaningful only on the authenticated connection generation they arrive on, an
announced delegation authenticates itself through its account signature, and a digest names a
value rather than asserting one; a duplicate or a stale one changes nothing. They are emitted
in a task of their own (refused, never run inline, when no runtime can carry one), never
awaited from the transport's read loop, and never through the transfer lanes. Each inbound
frame causes at most two of them, and at most 128 are in flight to one peer at a time, twice
the raw frames that peer may have in flight at this end's transport; a send beyond that budget
is dropped and repeated by the next frame that misses or teaches the same delegation. So what this
end spends on a peer's link control is bounded by the frames it accepts from that peer. The
sender's table and this budget survive a replacement of the outbound worker under an unchanged
generation.

## Expiry

An expired delegation is absent from both tables. A reference to it is a miss, and re-announcing
the expired delegation is refused by the same `Delegation::verify_delegator_authorization_at` that refuses it inline.
Expiry therefore forces a fresh delegation to be announced; it never resurrects an old one.

## Self-contained contexts

`MessagePayload::to_wire` and `from_wire` remain the self-contained encoding, both delegations
inline: handshake offers and answers exchanged out of band, the payload cut into chunks (it is
decoded after reassembly, outside the order of the link), and payloads held in a relay inbox.
A frame that references a delegation is refused there with `DelegationReferenceUnresolved`. So is a
frame on a connection from a peer other than the one its handshake is bound to: it arrived on no
link, and is judged as self-contained.

## Hard cutover

0.28.0 is a network-wide protocol cutover. Payload frames begin with the `RINGS-PAYLOAD` marker
and link-control frames with `RINGS-LINK`; a frame with neither, including every frame of
0.27.x and earlier, is rejected before deserialization. There is no dual decoder, negotiation,
or downgrade path.

The protocol is not versioned before 1.0. Markers, signing domains, AEAD namespaces, and storage
keys carry no version suffix: each of them names one domain, and a change to a domain is a
total cutover, not a new version beside an old one. The version suffixes earlier releases had
put on these names are removed in 0.28.0, so every signing domain (`rings-core:message-verification:transaction`,
`rings-core:message-verification:payload`, the inbox, receipt, descriptor and onion domains)
and the transaction-replay storage key change with it; a replay window persisted by 0.27.x is
not read, which loses nothing, since no 0.27.x frame is accepted either.
