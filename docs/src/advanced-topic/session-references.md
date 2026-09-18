# Session references on a link

Every `MessagePayload` carries two proofs: the origin's, over the transaction, and the current
hop's, over the carrier. Both are verified at every hop, not only at the final destination.
Each proof is a session-key signature plus the `Session` that delegates that key to an account:
about 150 bytes of delegation next to a 65-byte signature. The delegations are the same for the
lifetime of a connection, so since 0.28.0 a link sends each of them once and references it
afterwards.

## Wire form

A payload frame is the `MessagePayload` with each session slot replaced by a reference:

```text
SessionRef    = Inline(Session) | Digest(SessionDigest)
SessionDigest = the trailing 20 bytes of keccak256(encode(Session))
```

The digest is a content address of the whole delegation
`(session_id, account, ts_ms, ttl_ms, sig)`, not the session key's address. A session key can
carry more than one delegation: an account can re-delegate it under a new lifetime, and any
account can sign a delegation for a key it does not hold, because a delegation needs no proof of
possession. `session_id` therefore does not determine the account a message is attributed to;
the content address does. A resolved frame is exactly the payload that would have travelled
inline, so verification, origin attribution, and the transaction digest used for replay and
fork evidence do not depend on how a slot was encoded.

Neither signature covers the session slot (both sign the transaction hash), so a forwarding hop
re-encodes the origin slot for its own next link without touching the origin's signature.

## Scope: one link, one direction

A link is one admitted connection generation between two nodes. Each direction keeps one bounded
table of at most 64 sessions, least recently referenced first out:

| End | Table | Populated by |
| --- | --- | --- |
| sender | sessions it sent inline, and which of them the peer confirmed | its own frames; confirmations from the peer |
| receiver | sessions this link carried inline | frames that verified, and solicited announcements whose delegation verified |

Consequences of the scope:

- Only the peer at the other end of a connection can populate a receiver table, and only with
  its own bounded share. Nothing an unrelated party says is cached, and an announcement nobody
  asked for is ignored.
- A relay keeps the origin sessions of the traffic it forwards in the table of each outgoing
  link. A destination that never met the origin gets the origin's session inline from its last
  hop until it confirms it. No node ever asks the origin for anything.
- Both tables die with the connection generation. A restarted node and its peer start from
  empty tables together; there is no state to resynchronise.

## The link is a datagram link

Nothing assumes that frames arrive in order, or at all. WebRTC data channels happen to be
reliable and ordered; the protocol does not use that, so a transport built on QUIC datagrams or
plain UDP would carry it unchanged.

The sender switches a session from inline to reference only after the receiver has confirmed it:

```text
sender                                      receiver
  frame, session s inline        ───────▶   verifies; learns s
                                 ◀───────   Known(digest(s))
  frame, Digest(s)               ───────▶   resolves from its table
```

- Until the confirmation arrives, every frame carries `s` inline, and the receiver confirms
  every inline arrival of a session it knows. A lost inline frame, a lost confirmation, or any
  reordering costs inline frames, never a stall and never a miss: the reference is sent after
  the confirmation, which was sent after the session was learned.
- A reference therefore misses only when the receiver has *forgotten* the session (capacity
  eviction, expiry). That is repaired on the link.

## Miss

```text
receiver                                   sender
  frame references d, d unknown
  hold the frame; Request(d)   ───────▶   look d up in the sender table
                               ◀───────   Announce(session)  or  Unknown(d)
  announce: delegation verifies → release the frames that now resolve
  unknown / refused delegation  → fail the frames that await d
```

- Held frames are independent of each other and of everything else: a frame that resolves is
  never queued behind one that does not, because the link promises no order to preserve. A held
  frame leaves when a later frame or an announcement teaches the session it awaits, or is
  dropped when its proof lifetime lapses.
- The hold keeps at most 32 frames per connection. A digest is asked for when the first frame
  awaiting it is held; a frame that finds the hold full is dropped and every awaited digest is
  asked for again, since a full hold means an answer is overdue. There is no timer, and a peer
  that never answers stalls only its own held frames.
- The sender answers each question with exactly one frame, scheduled like any control transfer
  under the same per-peer and global capacity.

Link-control frames (`Known`, `Request`, `Announce`, `Unknown`) are unsigned and idempotent.
They are meaningful only on the authenticated connection they arrive on, an announced session
authenticates itself through its account signature, and a digest names a value rather than
asserting one; a duplicate or a stale one changes nothing.

## Expiry

An expired session is absent from both tables. A reference to it is a miss, and re-announcing
the expired delegation is refused by the same `Session::verify_self_at` that refuses it inline.
Expiry therefore forces a fresh delegation to be announced; it never resurrects an old one.

## Self-contained contexts

`MessagePayload::to_wire` and `from_wire` remain the self-contained encoding, both sessions
inline: handshake offers and answers exchanged out of band, the payload cut into chunks (it is
decoded after reassembly, outside the order of the link), and payloads held in a relay inbox.
A frame that references a session is refused there with `SessionReferenceUnresolved`.

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
