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
table:

| End | Table | Capacity | Populated by |
| --- | --- | --- | --- |
| sender | delegations it sent inline, and the lease the peer granted each | 64 | its own frames; leases from the peer |
| receiver | delegations this link carried inline, each under a lease it granted | 128 | frames that verified |

The sender's table is a least-recently-referenced cache: what it forgets travels inline again.
The receiver's table is not a cache. It never evicts a live lease to make room, and 8 of its
128 entries are reserved for the link peer's own (hop) delegations, so relayed origins cannot
push the peer's own session inline. A full table refuses to lease a new delegation, and the
sender keeps sending it inline, which is always admissible.

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
                                 ◀───────   Known(digest(s), until)
  frame, Digest(s)               ───────▶   resolves from its table
```

Until the lease arrives, every frame carries `s` inline, and the receiver confirms every inline
arrival that verified and that it holds. A lost inline frame, a lost lease, or any reordering
costs inline frames, never a stall.

## Lease

The receiver leases `s` only once `s` is resident in its table, with
`Known(digest(s), until)`, where `until = min(exp(s), now + 5 min)` on the receiver's clock.
Write `Δ` and `G` for one delivery bound each (25 s).

- The receiver keeps `s` until `until + G`. `until` never decreases.
- The sender references `s` only while `now + Δ < until_S`, where `until_S` is the largest
  `until` it was sent. Otherwise it sends `s` inline.
- A reference that finds less than half a lease left renews it, and the receiver sends the
  new `Known`.

The sender learns only values the receiver granted, and a stale `Known` carries an older,
smaller `until`, so `until_S ≤ until_R`. A reference therefore resolves whenever its transit
plus the two clocks' skew stays under `Δ + G`, on every interleaving and every loss. An unused
lease leaves the receiver's table within 5 min plus `G`, which bounds what one link pins.

## Miss

A reference the receiver does not hold is a miss. On an honest link it can only come from this
end's clock or a fault, so it is not charged:

```text
receiver                                   sender
  frame references d, d not held
  drop the frame, uncharged; Unknown(d)  ───────▶   d travels inline again
```

Nothing is held. A dishonest sender gains nothing by missing: its own frame is dropped. Since no
frame waits on a repair, the frames of one lane reach the logical stage in their arrival order
(#908; see [Transaction Replay Protection](transaction-replay.md)).

Link-control frames (`Known`, `Unknown`) are unsigned and idempotent. They are meaningful only
on the authenticated connection generation they arrive on, a lease is granted only for a
delegation that verified, and a digest names a value rather than asserting one; a duplicate or a
stale one changes nothing. They travel on the DHT-control lane, in a task of their own (refused,
never run inline, when no runtime can carry one), never awaited by the transport lane whose frame
caused them. A send waits for its lane's credit at most one delivery bound, and is then dropped:
a lost lease is granted again by the next inline frame or renewing reference, a lost disclaimer
is sent again by the next miss. Each inbound frame causes at most two of them, and at most 128
are in flight to one peer at a time, twice the raw frames that peer may have in flight at this
end's transport; a send beyond that budget is dropped and repeated by the next frame that misses
or teaches the same delegation. So what this end spends on a peer's link control is bounded by
the frames it accepts from that peer. The sender's table and this budget survive a replacement
of the outbound worker under an unchanged generation.

## Expiry

An expired delegation is absent from both tables, and a lease never outlives its delegation.
The sender refuses to send any frame with a delegation that expires within `Δ`, so no frame needs
a delegation past its expiry in flight. An origin delegation that expired in flight, within `G`
of arrival, is refused without charging the hop that carried it: the hop answers only for its
own delegation. A fresh delegation must travel inline and verify like any other; expiry never
resurrects an old one.

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
