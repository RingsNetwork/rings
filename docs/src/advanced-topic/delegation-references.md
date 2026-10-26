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
inline, so the verification verdict, origin attribution, and the transaction digest used for
replay and fork evidence do not depend on how a slot was encoded. Only the work does: a
referenced slot resolves from the lessor table, whose every entry was leased from a frame that
verified in full, so its delegator's signature is not checked again; its expiry and the
delegatee's signature are (#1089).

Neither signature covers the delegation slot (both sign the transaction hash), so a forwarding hop
re-encodes the origin slot for its own next link without touching the origin's signature.

## Scope: one link, one direction

A link is one admitted connection generation between two nodes. Each direction keeps one bounded
table:

| End | Table | Capacity | Populated by |
| --- | --- | --- | --- |
| sender | delegations it sent inline, and the lease the peer granted each | 64 | its own frames; leases from the peer |
| receiver | delegations this link carried inline, each under a lease it granted | 128 | frames that verified |

The sender's table is a least-recently-referenced cache: what it forgets travels inline again.
The receiver's table is not a cache. It never reclaims a lease before its grace ends (see
[Lease](#lease)), and 8 of its 128 entries are reserved for the link peer's own (hop)
delegations, so relayed origins cannot push the peer's own session inline. A table full of
leases still within their grace refuses to lease a new delegation, and the sender keeps sending
it inline, which is always admissible.

Consequences of the scope:

- Only the peer at the other end of a connection can populate a receiver table, and only with
  delegations of its own frames that verified. Nothing an unrelated party says is cached.
- A relay keeps the origin delegations of the traffic it forwards in the table of each outgoing
  link. A destination that never met the origin gets the origin's delegation inline from its last
  hop until it leases it. No node ever asks the origin for anything.
- Both tables are scoped to the connection generation: the receiver's lives in the callback
  of one connection, the sender's is emptied by the first frame of a newer generation (and
  survives a replacement of the outbound worker under an unchanged generation). A restarted
  node and its peer start from empty tables together; there is no state to resynchronise, and
  a control frame is emitted only on the generation it was judged on, so a late one never
  leases or disclaims on a generation whose tables never saw it.
- Only frames on the link teach the link's table, and what a verified frame teaches is leased
  and confirmed to the peer before the frame itself is gated for admission. A frame from any
  other peer, or on a callback bound to no handshake, is judged self-contained: a reference in
  it is refused, and it teaches nothing.

## The link is a datagram link

Nothing assumes that frames arrive in order, or at all. WebRTC data channels happen to be
reliable and ordered; the protocol does not use that, so a transport built on QUIC datagrams or
plain UDP would carry it unchanged.

The sender switches a delegation from inline to reference only under a lease the receiver granted:

```text
sender                                      receiver
  frame, delegation s inline        ───────▶   verifies; leases s
                                 ◀───────   Known(digest(s), r)
  frame, Digest(s)               ───────▶   resolves from its table
```

Until the lease arrives, every frame carries `s` inline, and the receiver confirms every inline
arrival that verified and that it holds. A lost inline frame, a lost lease, or any reordering
costs inline frames, never a stall.

## Lease

Write `Δ` (the margin) and `G` (the grace) for one delivery bound each: 25 s in production.

The receiver leases `s` only once a frame carrying it inline verified and `s` is resident in its
table. It sends `Known(digest(s), r)`, where `r = until − now` and
`until = min(exp(s), now + 5 min)` on the receiver's clock. `r` is a remaining duration, not an
instant, so no clock is ever compared with another:

- The sender, receiving `Known(digest(s), r)` at `t_S` on its own clock, records
  `until_S = t_S + r`, keeping the largest such value it received. In real time, whatever the
  two clocks read, `until_S` exceeds the receiver's `until` by exactly the transit `c` of the
  `Known` it came from; a stale `Known` counts from its own, older grant and never lengthens a
  newer one.
- The sender references `s` only while `now + Δ < until_S`. Otherwise it sends `s` inline.
- The receiver never reclaims a lease before `until + G`, and `until` never decreases.

A reference of transit `t` therefore resolves whenever `c + t ≤ Δ + G`, the confirmation and
the reference each within one delivery bound, on every interleaving and every loss.

**Residence.** Whether a reference resolves depends only on whether the delegation is resident
in the receiver's table, never on when the frame arrived or how long it waited. A lease that
ended, grace included, is reclaimed lazily: only when its slot is needed for a new lease. Until
then a reference to it still resolves, so a frame that waits in its lane at the receiver
resolves however long it waited, unless the table filled up meanwhile. An unused lease becomes
reclaimable 5 min plus `G` after its last grant or renewal, which bounds what one link pins.

**Renewal.** Only a frame that verified renews a lease. When a verified frame references a
delegation with less than half a lease left, the receiver renews it, if that lengthens it, and
sends the new `Known`. A verified frame causes at most one lease or renewal per distinct
delegation it carries: a peer's own frame, whose origin is its hop, carries one.

## Miss

A reference the receiver does not hold is a miss. On an honest link it can only come from a
confirmation and a reference whose transits together exceed `Δ + G`, under a full table, or a fault at this end, so it is not charged:

```text
receiver                                   sender
  frame references d, d not held
  drop the frame, uncharged; Unknown(d)  ───────▶   d travels inline again
```

Nothing is held. A dishonest sender gains nothing by missing: its own frame is dropped. Since no
frame waits on a repair, the frames of one lane reach the logical stage in their arrival order
(#908; see [Transaction Replay Protection](transaction-replay.md)). A stale `Unknown` that
overtakes a newer `Known` costs inline frames until the next lease, never a miss.

Link-control frames (`Known`, `Unknown`) are unsigned and idempotent. They are meaningful only
on the authenticated connection generation they arrive on, a lease is granted only for a
delegation that verified, and a digest names a value rather than asserting one; a duplicate
changes nothing. They travel on the DHT-control lane, in a task of their own (refused, never
run inline, when no runtime can carry one), never awaited by the transport lane whose frame
caused them. A send waits for its lane's credit at most one delivery bound, and is then
dropped: a lost lease is granted again by the next inline frame or renewing reference, a lost
disclaimer is sent again by the next miss. Each inbound frame causes at most two of them, one
per distinct delegation slot, and at most 128 are in flight to one peer at a time, twice the
raw frames that peer may have in flight at this end's transport; a send beyond that budget is
dropped and repeated by the next frame that misses or teaches the same delegation. So what this
end spends on a peer's link control is bounded by the frames it accepts from that peer. The
sender's table and this budget survive a replacement of the outbound worker under an unchanged
generation.

## Expiry

The sender's table holds no expired delegation, and a receiver's lease never outlives its
delegation. The delegation itself may outlive its lease at the receiver: it stays resident
through the grace `G`, and after that until its slot is needed (the residence law). Residence
grants nothing, though: verification refuses an expired delegation, so a frame that resolves to
one is refused like any other expired frame. A fresh delegation must travel inline and verify
like any other; expiry never resurrects an old one.

The sender refuses to send a frame with a delegation that expires within `Δ`, so no frame needs
a delegation past its expiry in flight. The refusal is the local error
`DelegationExpiresWithinMargin`: nothing reached the peer, so it is not charged to the next hop.

An origin delegation can still expire in flight, after the hop that carried the frame checked
it. Such a frame is dropped without charging that hop, since the hop answers only for its own
delegation, but only when all of these hold:

- the origin is not the hop;
- the origin delegation expired at most `G` before arrival;
- the hop's proof verifies now;
- the origin's proof verifies at the instant its delegation expired.

Any other failure, a forged or otherwise invalid frame included, is charged as before.

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

#908 changes the link-control tags again: `Request` and `Announce` are removed, and `Known`
carries the remaining duration of a lease (`remaining_ms`) instead of an instant on the
receiver's clock. A link-control frame of the previous encoding is not decoded; both ends of a
link upgrade together.

The protocol is not versioned before 1.0. Markers, signing domains, AEAD namespaces, and storage
keys carry no version suffix: each of them names one domain, and a change to a domain is a
total cutover, not a new version beside an old one. The version suffixes earlier releases had
put on these names are removed in 0.28.0, so every signing domain (`rings-core:message-verification:transaction`,
`rings-core:message-verification:payload`, the inbox, receipt, descriptor and onion domains)
and the transaction-replay storage key change with it; a replay window persisted by 0.27.x is
not read, which loses nothing, since no 0.27.x frame is accepted either.
