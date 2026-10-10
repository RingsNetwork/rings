//! Constant variables.

use std::num::NonZeroU32;

/// Default time-to-live in milliseconds, shared by message signatures and DHT entries.
///
/// A message proof is live for this long after its timestamp; a DHT entry stamped at the
/// operation boundary is retained for this long after it was issued.
pub const DEFAULT_TTL_MS: u64 = 600 * 1000;
/// Maximum accepted time-to-live in milliseconds for message signatures and DHT entries.
pub const MAX_TTL_MS: u64 = DEFAULT_TTL_MS * 10;
/// Default retention in milliseconds of a relay inbox, the messages held for an offline peer.
///
/// A peer that returns within this window after the last message held for it still receives the
/// whole inbox. The policy is safe only because every inbox element carries a witness the
/// storage owner verifies itself (see `dht::entry::inbox`).
pub const DEFAULT_RELAY_INBOX_TTL_MS: u64 = 24 * 3600 * 1000;
/// Maximum accepted retention in milliseconds of a relay inbox.
pub const MAX_RELAY_INBOX_TTL_MS: u64 = DEFAULT_RELAY_INBOX_TTL_MS * 7;
/// Accepted timestamp drift in milliseconds.
pub const TS_OFFSET_TOLERANCE_MS: u128 = 3000;
/// The forwards a fresh relay carrier holds, and the most any carrier can hold.
///
/// A greedy Chord route over `N` nodes with a finger table spanning the identifier space takes
/// about `log2 N` finger hops, so this covers overlays far wider than any the relay routes over.
/// A ring whose finger table does not span the space (the small tables of simulated networks)
/// routes by successor walk instead, whose length is the ring size and is known to no node, so
/// the budget is a network constant rather than a per-ring derivation; it covers such rings up
/// to 65 nodes. Delivery toward a node never cycles (see `dht::delivery`): a route takes at most
/// `2|V|` hops, so the budget ends one only when such a correct route is longer than it.
pub const MAX_RELAY_HOPS: u8 = 64;
/// Maximum number of fetched entries the local DHT cache retains before evicting the
/// least recently written one.
pub const LOCAL_CACHE_CAPACITY: NonZeroU32 = match NonZeroU32::new(1024) {
    Some(capacity) => capacity,
    None => unreachable!(),
};
/// Default session time-to-live in milliseconds.
pub const DEFAULT_DELEGATION_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// Ceiling on one logical message, 60 MB. The data-channel frame size is not
/// derived from it: the transport negotiates `max_message_size` per connection
/// and the chunk layer sizes frames to that.
pub const TRANSPORT_MAX_SIZE: usize = 60_000_000;
/// Bytes the transport adds when it serializes the data-channel frame: every send is wrapped in
/// `rings_codec::serialize(TransportMessage::Custom(bytes))` before it reaches SCTP, and the
/// framing decision accounts for this outer wrapper so a payload sized at the limit still fits
/// once wrapped.
///
/// Derivation: the enum tag (1 byte) and the varint length prefix of a frame of at most
/// `MAX_DATA_CHANNEL_MESSAGE_SIZE` bytes (3 bytes), `1 + 3 = 4`; the supremum, witnessed with
/// equality by `test_chunk_envelope_reserves_are_the_widest_framed_chunk`.
pub const TRANSPORT_CUSTOM_OVERHEAD: usize = 4;
/// Bytes reserved, per chunk, for the `MessagePayload` envelope a chunk is re-wrapped in before
/// sending, *not* counting the outer [`TRANSPORT_CUSTOM_OVERHEAD`], which is added separately.
/// The chunk *data* size is the connection's negotiated `max_message_size` minus both reserves,
/// so the wrapped on-wire message stays within the data-channel limit.
///
/// Derivation: the encoded size, less its chunk data, of the widest frame the chunk framer
/// (`frame_chunk`) emits. The fields it fixes are as it fixes them: no `reply_via`, the relay
/// aim toward the receiver, an exhausted hop budget. Every field it leaves free takes its
/// widest encoding: both delegation slots inline with the widest delegation (a BLS12-381
/// account key, 48 bytes, and its 96-byte signature, the longest any verified delegation
/// carries; `SignatureAlgorithm::signature_len`), every timestamp, lifetime and sequence at its
/// type's maximum, the chunk header at the most chunks one message is cut into
/// (`TRANSPORT_MAX_SIZE / MIN_CHUNK_DATA`), and the length prefixes of
/// `MAX_DATA_CHANNEL_MESSAGE_SIZE` data bytes. The value is that supremum, witnessed with
/// equality through `frame_chunk` itself by
/// `test_chunk_envelope_reserves_are_the_widest_framed_chunk`: a larger reserve wastes the same
/// bytes in every chunk frame (#925), a smaller one lets a frame overflow the channel. A field
/// the framer sets differently changes the witness, which then re-derives this value.
pub const MAX_CHUNK_ENVELOPE_OVERHEAD: usize = 911;
/// Bytes a whole `MessagePayload` encodes beyond its message's own encoding, at most: the
/// envelope reserve of a payload sent unchunked, such as a storage hand-off batch.
///
/// Derivation: every envelope field at its widest, both delegation slots with the widest
/// delegation, `reply_via` and the relay aim present (a relayed payload may carry both), and
/// the length prefix of a message of up to `MAX_DATA_CHANNEL_MESSAGE_SIZE` bytes; the
/// supremum, witnessed with equality by `test_payload_envelope_reserve_is_the_widest_envelope`.
pub const MAX_PAYLOAD_ENVELOPE_OVERHEAD: usize = 941;
/// Smallest per-chunk *data* payload we are willing to produce. A peer that advertises a
/// `max_message_size` so small that, after the envelope reserves, fewer than this many data bytes
/// fit per chunk is rejected outright (`WireReserves::plan` returns `None`) rather than fragmenting a
/// message into a huge number of near-empty chunks. This bounds the chunk count for any payload:
/// at most `TRANSPORT_MAX_SIZE / MIN_CHUNK_DATA` chunks.
pub const MIN_CHUNK_DATA: usize = 1024;
/// Maximum number of encoded payloads kept in a data topic.
pub const ENTRY_DATA_MAX_LEN: usize = 1024;
/// Maximum number of held messages kept in a relay inbox.
pub const RELAY_INBOX_MAX_LEN: usize = 64;
/// Maximum bytes of one element in a DHT storage entry.
///
/// The bound is per element so that filtering by it is a lattice morphism; with
/// [`ENTRY_DATA_MAX_LEN`] it bounds every carrier at `ENTRY_DATA_MAX_LEN * ENTRY_PAYLOAD_MAX_BYTES`.
pub const ENTRY_PAYLOAD_MAX_BYTES: usize = 32 * 1024;
/// Carrier law: a full carrier must still be one transport message, since a hand-off, a
/// republish, and a lookup answer each carry a whole carrier. Elements are already in their
/// wire encoding; the carrier's wire form adds only codec framing, dots, and the message
/// envelope, for which a quarter of [`TRANSPORT_MAX_SIZE`] is reserved.
const _: () = assert!(
    ENTRY_DATA_MAX_LEN * ENTRY_PAYLOAD_MAX_BYTES <= TRANSPORT_MAX_SIZE / 4 * 3,
    "a full carrier must fit one transport message with room for its envelope"
);
