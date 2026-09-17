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
| sender | sessions this link carried inline | frames the transport accepted, in acceptance order |
| receiver | sessions this link carried inline | frames that verified, and solicited announcements whose delegation verified |

Consequences of the scope:

- Only the peer at the other end of a connection can populate a receiver table, and only with
  its own bounded share. Nothing an unrelated party says is cached, and an announcement nobody
  asked for is ignored.
- A relay keeps the origin sessions of the traffic it forwards in the table of each outgoing
  link. A destination that never met the origin gets the origin's session inline in the first
  frame its last hop forwards for that origin. No node ever asks the origin for anything.
- Both tables die with the connection generation. A restarted node and its peer start from
  empty tables together; there is no state to resynchronise.
- References are used only over sequenced delivery (a reliable, ordered data channel). A
  transport that may reorder or drop accepted frames while the connection stays up carries
  every frame self-contained.

## Miss

The tables agree when frames arrive in the order they were accepted for sending. That is an
optimisation, not an assumption. A digest the receiver cannot resolve is repaired on the link:

```text
receiver                                   sender
  frame references d, d unknown
  hold frame (and frames behind it)
  Request(d)                   ───────▶   look d up in the sender table
                               ◀───────   Announce(session)  or  Unknown(d)
  announce: delegation verifies → release held frames in arrival order
  unknown / refused delegation  → fail the frames that await d, release the rest
```

- The hold keeps at most 32 frames per connection, in arrival order; the frame that would
  exceed it is dropped, and its arrival asks the head's question again.
- Only the head's missing digests are requested, once per drain attempt. Drain attempts are
  caused by arrivals and answers; there is no timer. A peer that never answers stalls only its
  own link, and a held frame whose proof lifetime lapses is dropped.
- The sender answers each question with exactly one frame, scheduled like any control transfer
  under the same per-peer and global capacity.

Link-control frames are unsigned. They are meaningful only on the authenticated connection they
arrive on, an announced session authenticates itself through its account signature, and a
digest names a value rather than asserting one.

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

0.28.0 is a network-wide protocol cutover. Payload frames begin with the `RINGS-PAYLOAD-V3`
marker and link-control frames with `RINGS-LINK-V1`; a frame with neither, including every
`RINGS-TX-V2` frame of 0.24.0 to 0.27.x, is rejected before deserialization. There is no dual
decoder, negotiation, or downgrade path. Transaction signatures and their signing domains are
unchanged.
