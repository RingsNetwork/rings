//! Constant variables.
///
/// default ttl in ms
pub const DEFAULT_TTL_MS: u64 = 600 * 1000;
pub const MAX_TTL_MS: u64 = DEFAULT_TTL_MS * 10;
pub const TS_OFFSET_TOLERANCE_MS: u128 = 3000;
pub const DEFAULT_SESSION_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// 60k
pub const TRANSPORT_MTU: usize = 60000;
/// 60M
pub const TRANSPORT_MAX_SIZE: usize = TRANSPORT_MTU * 1000;
/// Bytes the transport adds when it serializes the data-channel frame: every send is wrapped in
/// `bincode(TransportMessage::Custom(bytes))` (an enum tag + a length prefix) before it reaches
/// SCTP. The framing decision must account for this outer wrapper, not just the inner payload, or a
/// payload sized exactly at the limit would overflow once wrapped. Generous bound on that framing.
pub const TRANSPORT_CUSTOM_OVERHEAD: usize = 64;
/// Bytes reserved, per chunk, for the `MessagePayload` envelope a chunk is re-wrapped in before
/// sending (signature, DIDs, relay, bincode framing) — *not* counting the outer
/// [`TRANSPORT_CUSTOM_OVERHEAD`], which is added separately. The chunk *data* size is the
/// connection's negotiated `max_message_size` minus both reserves, so the wrapped on-wire message
/// stays within the data-channel limit. Generous; bounded by the `chunk_envelope_fits_reserve` test.
pub const MAX_CHUNK_ENVELOPE_OVERHEAD: usize = 4096;
/// Smallest per-chunk *data* payload we are willing to produce. A peer that advertises a
/// `max_message_size` so small that, after the envelope reserves, fewer than this many data bytes
/// fit per chunk is rejected outright (`WireReserves::plan` returns `None`) rather than fragmenting a
/// message into a huge number of near-empty chunks. This bounds the chunk count for any payload:
/// at most `TRANSPORT_MAX_SIZE / MIN_CHUNK_DATA` chunks.
pub const MIN_CHUNK_DATA: usize = 1024;
pub const VNODE_DATA_MAX_LEN: usize = 1024;
