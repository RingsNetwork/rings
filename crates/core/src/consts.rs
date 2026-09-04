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
/// Maximum number of fetched entries the local DHT cache retains before evicting the
/// least recently written one.
pub const LOCAL_CACHE_CAPACITY: NonZeroU32 = match NonZeroU32::new(1024) {
    Some(capacity) => capacity,
    None => unreachable!(),
};
/// Default session time-to-live in milliseconds.
pub const DEFAULT_SESSION_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// 60k
pub const TRANSPORT_MTU: usize = 60000;
/// 60M
pub const TRANSPORT_MAX_SIZE: usize = TRANSPORT_MTU * 1000;
/// Bytes the transport adds when it serializes the data-channel frame: every send is wrapped in
/// `rings_codec::serialize(TransportMessage::Custom(bytes))` before it reaches SCTP. The framing
/// decision must account for this outer wrapper, not just the inner payload, or a payload sized
/// exactly at the limit would overflow once wrapped. Generous bound on that framing.
pub const TRANSPORT_CUSTOM_OVERHEAD: usize = 64;
/// Bytes reserved, per chunk, for the `MessagePayload` envelope a chunk is re-wrapped in before
/// sending (signature, DIDs, relay, codec framing) — *not* counting the outer
/// [`TRANSPORT_CUSTOM_OVERHEAD`], which is added separately. The chunk *data* size is the
/// connection's negotiated `max_message_size` minus both reserves, so the wrapped on-wire message
/// stays within the data-channel limit. Generous; bounded by the
/// `test_chunk_envelope_fits_reserve` test.
pub const MAX_CHUNK_ENVELOPE_OVERHEAD: usize = 4096;
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
/// Maximum encoded bytes of one payload element in a DHT storage entry.
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
