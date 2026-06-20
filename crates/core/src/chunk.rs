#![warn(missing_docs)]
//! Message framing / chunking, inspired by RFC 4975 (MSRP) chunking
//! <https://www.rfc-editor.org/rfc/rfc4975#page-9>: a large message is split into
//! MTU-sized [`Chunk`]s on the sender and reassembled on the receiver, so a big payload
//! does not monopolise a connection.
//!
//! Two halves, deliberately separated:
//!
//! - **Send** — [`ChunkList`] splits a [`Bytes`] into ordered [`Chunk`]s
//!   (`ChunkList::split(&bytes, chunk_size)`, where `chunk_size` comes from the connection's
//!   negotiated `max_message_size`), which the caller iterates and puts on the wire.
//! - **Receive** — [`ChunkReassembler`] collects incoming [`Chunk`]s keyed by message id and
//!   yields the original payload once every position has arrived.
//!
//! The receiver is robust to the realities of a multi-hop / DHT overlay: out-of-order arrival,
//! **duplicates / retransmits** (first write per position wins), and partial messages (evicted
//! by TTL). It is also bounded — at most [`MAX_PENDING_MESSAGES`] incomplete messages are held,
//! and only the positions actually received are buffered (no allocation proportional to a
//! peer-supplied `total`).
//!
//! ```text
//!   send    : Bytes ↦ [Chunk{ chunk=[i, n], data=dataᵢ, meta } | i ∈ 0..n]
//!   receive : a message id is complete ⟺ {position | chunk received} = {0..total-1};
//!             then payload = concat(dataᵢ for i ∈ 0..total)
//! ```

use std::collections::btree_map::BTreeMap;
use std::collections::HashMap;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::error::Error;
use crate::error::Result;
use crate::utils::get_epoch_ms;

/// Upper bound on concurrently-reassembling (incomplete) messages held by a
/// [`ChunkReassembler`]. Once reached, expired messages are reclaimed and any *new* message is
/// dropped — so a lossy or malicious peer cannot grow the buffer without bound (DoS guard).
pub const MAX_PENDING_MESSAGES: usize = 512;

/// One chunk of a chunked message, as it travels on the wire.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunk {
    /// `[position, total]` — this chunk's index and the number of chunks in the message.
    pub chunk: [usize; 2],
    /// chunk payload bytes
    pub data: Bytes,
    /// meta data of chunk
    pub meta: ChunkMeta,
}

impl Chunk {
    /// serialize chunk to bytes
    pub fn to_bincode(&self) -> Result<Bytes> {
        bincode::serialize(self)
            .map(Bytes::from)
            .map_err(Error::BincodeSerialize)
    }

    /// deserialize bytes to chunk
    pub fn from_bincode(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data).map_err(Error::BincodeDeserialize)
    }
}

/// Meta data of a chunk
#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub struct ChunkMeta {
    /// uuid of msg
    pub id: uuid::Uuid,
    /// Created time
    pub ts_ms: u128,
    /// Time to live
    pub ttl_ms: u64,
}

impl Default for ChunkMeta {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            ts_ms: get_epoch_ms(),
            ttl_ms: DEFAULT_TTL_MS,
        }
    }
}

/// Sender side: an ordered list of [`Chunk`]s for one message. Build it from the payload with
/// [`ChunkList::split`], passing the per-message data size to cut at (the connection's negotiated
/// `max_message_size` minus the envelope reserve), then iterate (or convert to `Vec<Chunk>`) to put
/// each chunk on the wire. The cut size is a runtime argument rather than a type parameter because
/// it is decided per connection from the negotiated limit. Reassembly is the receiver's job — see
/// [`ChunkReassembler`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChunkList(Vec<Chunk>);

impl ChunkList {
    /// Split `bytes` into chunks of at most `chunk_size` data bytes each, tagged `[i, total]` so the
    /// receiver can reassemble them in order. `chunk_size` is clamped to at least 1 so a degenerate
    /// limit still terminates (one byte per chunk) rather than dividing by zero.
    pub fn split(bytes: &Bytes, chunk_size: usize) -> Self {
        let chunk_size = chunk_size.max(1);
        let chunks: Vec<Bytes> = bytes
            .chunks(chunk_size)
            .map(|c| c.to_vec().into())
            .collect();
        let chunks_len: usize = chunks.len();
        let meta = ChunkMeta::default();
        Self(
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, data)| Chunk {
                    meta,
                    chunk: [i, chunks_len],
                    data,
                })
                .collect::<Vec<Chunk>>(),
        )
    }

    /// Clone out the chunks.
    pub fn to_vec(&self) -> Vec<Chunk> {
        self.0.clone()
    }

    /// Borrow the chunks.
    pub fn as_vec(&self) -> &Vec<Chunk> {
        &self.0
    }
}

impl IntoIterator for &ChunkList {
    type Item = Chunk;
    type IntoIter = std::vec::IntoIter<Chunk>;

    fn into_iter(self) -> Self::IntoIter {
        self.to_vec().into_iter()
    }
}

impl IntoIterator for ChunkList {
    type Item = Chunk;
    type IntoIter = std::vec::IntoIter<Chunk>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl From<ChunkList> for Vec<Chunk> {
    fn from(l: ChunkList) -> Self {
        l.0
    }
}

impl From<Vec<Chunk>> for ChunkList {
    fn from(data: Vec<Chunk>) -> Self {
        Self(data)
    }
}

/// How one payload should be framed for a size-limited connection: sent whole, or split.
///
/// This is the *decision* only — a value, with no I/O — so the sender's effectful path
/// (`do_send_payload`) is a thin shell that matches on it. Separating the rule from the act keeps
/// the rule exhaustively testable in isolation (functional core / imperative shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// The payload is within the connection's limit; send it as a single message, unchanged.
    Whole,
    /// The payload exceeds the limit; split it into [`Chunk`]s of at most `chunk_size` data bytes
    /// each (via [`ChunkList::split`]), each then re-wrapped in its own envelope.
    Chunked {
        /// Maximum data bytes per chunk.
        chunk_size: usize,
    },
}

/// Decide how to frame a `payload_len`-byte payload for a connection whose negotiated per-message
/// limit is `max_message_size`, reserving `envelope_overhead` bytes per chunk for the envelope each
/// chunk is re-wrapped in before sending.
///
/// A pure total function of three lengths:
///
/// ```text
///   plan : (len, limit, reserve) ↦ Whole                          if len ≤ limit
///                                  Chunked{ max(1, limit − reserve) }  otherwise
/// ```
///
/// We chunk only when the payload itself exceeds the limit, and cut chunk data at
/// `limit − reserve` so each wrapped chunk still fits the limit. The cut is floored at 1 so a
/// degenerate `limit ≤ reserve` still makes progress (one data byte per chunk) instead of cutting
/// at zero and never terminating.
pub fn plan_framing(
    payload_len: usize,
    max_message_size: usize,
    envelope_overhead: usize,
) -> Framing {
    if payload_len <= max_message_size {
        Framing::Whole
    } else {
        let chunk_size = max_message_size.saturating_sub(envelope_overhead).max(1);
        Framing::Chunked { chunk_size }
    }
}

/// One message being reassembled: the chunks seen so far, keyed by position.
struct Pending {
    /// total number of chunks the message claims (from `chunk[1]`).
    total: usize,
    /// received positions → bytes. A `BTreeMap` dedups by position (first write wins) and keeps
    /// the data ordered, so assembly is a single in-order concat.
    slots: BTreeMap<usize, Bytes>,
    /// creation time / ttl of the first chunk seen, used for TTL eviction.
    ts_ms: u128,
    ttl_ms: u64,
}

impl Pending {
    fn new(total: usize, ts_ms: u128, ttl_ms: u64) -> Self {
        Self {
            total,
            slots: BTreeMap::new(),
            ts_ms,
            ttl_ms,
        }
    }

    /// Complete iff every position has arrived. Each inserted position is unique (map key) and in
    /// `0..total`, so `slots.len() == total` ⟺ the present set is exactly `{0..total-1}`.
    fn is_complete(&self) -> bool {
        self.slots.len() == self.total
    }

    fn assemble(self) -> Bytes {
        self.slots.into_values().flatten().collect()
    }
}

/// Receiver side: reassembles [`Chunk`]s into whole messages, keyed by message id.
///
/// Correct under duplicates / retransmits (first write per position wins), out-of-order arrival
/// (positions are sorted), and partial delivery (TTL eviction). Bounded by
/// [`MAX_PENDING_MESSAGES`], and it only buffers the positions actually received (a peer-supplied
/// `total` never drives an allocation).
#[derive(Default)]
pub struct ChunkReassembler {
    pending: HashMap<Uuid, Pending>,
}

impl ChunkReassembler {
    /// Empty reassembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of messages currently being reassembled (incomplete).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Drop messages whose TTL has elapsed.
    pub fn remove_expired(&mut self) {
        let now = get_epoch_ms();
        self.pending.retain(|_, p| p.ts_ms + p.ttl_ms as u128 > now);
    }

    /// Forget a message (e.g. after it has been delivered).
    pub fn remove(&mut self, id: Uuid) {
        self.pending.remove(&id);
    }

    /// Accept one chunk. Returns the fully reassembled payload when this chunk completes its
    /// message (which is then forgotten), otherwise `None`. Malformed, too-old, or
    /// over-the-cap chunks are dropped (`None`).
    pub fn handle(&mut self, chunk: Chunk) -> Option<Bytes> {
        // Reject an absurd ttl outright.
        if chunk.meta.ttl_ms > MAX_TTL_MS {
            return None;
        }
        // Reject a chunk stamped too far in the future. `saturating_sub` avoids the `u128`
        // underflow a malformed/forged `ts_ms < TS_OFFSET_TOLERANCE_MS` would otherwise cause.
        if chunk.meta.ts_ms.saturating_sub(TS_OFFSET_TOLERANCE_MS) > get_epoch_ms() {
            return None;
        }

        let [position, total] = chunk.chunk;
        // A real message has at least one chunk and every position is in `0..total`.
        if total == 0 || position >= total {
            return None;
        }

        self.remove_expired();

        let id = chunk.meta.id;
        // Bound concurrent messages: once at the cap (after reclaiming expired ones above), drop
        // any *new* message rather than grow without limit.
        if !self.pending.contains_key(&id) && self.pending.len() >= MAX_PENDING_MESSAGES {
            return None;
        }

        let pending = self
            .pending
            .entry(id)
            .or_insert_with(|| Pending::new(total, chunk.meta.ts_ms, chunk.meta.ttl_ms));

        // A chunk whose `total` disagrees with the first one seen for this id is malformed; ignore
        // it rather than corrupt the in-flight message.
        if pending.total != total {
            return None;
        }

        // First write per position wins — a duplicate/retransmitted position is a no-op.
        pending.slots.entry(position).or_insert(chunk.data);

        if pending.is_complete() {
            // `remove` returns the owned `Pending`; assemble in position order.
            return self.pending.remove(&id).map(Pending::assemble);
        }
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn chunks_of(data: &Bytes, mtu: usize) -> Vec<Chunk> {
        ChunkList::split(data, mtu).into()
    }

    #[test]
    fn test_data_chunks() {
        let data = "helloworld".repeat(2).into();
        let ret: Vec<Chunk> = ChunkList::split(&data, 32).into();
        assert_eq!(ret.len(), 1);
        assert_eq!(ret[ret.len() - 1].chunk, [0, 1]);

        let data = "helloworld".repeat(1024).into();
        let ret: Vec<Chunk> = ChunkList::split(&data, 32).into();
        assert_eq!(ret.len(), 10 * 1024 / 32);
        assert_eq!(ret[ret.len() - 1].chunk, [319, 320]);
    }

    #[test]
    fn split_empty_yields_no_chunks() {
        assert!(ChunkList::split(&Bytes::new(), 32).to_vec().is_empty());
    }

    #[test]
    fn split_exact_multiple_all_full() {
        let data: Bytes = vec![0u8; 64].into();
        let chunks = ChunkList::split(&data, 32).to_vec();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.data.len() == 32));
        assert_eq!(chunks[0].chunk, [0, 2]);
        assert_eq!(chunks[1].chunk, [1, 2]);
    }

    #[test]
    fn split_non_multiple_last_is_remainder() {
        let data: Bytes = vec![0u8; 70].into();
        let chunks = ChunkList::split(&data, 32).to_vec();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].data.len(), 32);
        assert_eq!(chunks[1].data.len(), 32);
        assert_eq!(chunks[2].data.len(), 6);
    }

    #[test]
    fn split_larger_than_data_is_single_chunk() {
        let data: Bytes = vec![0u8; 10].into();
        let chunks = ChunkList::split(&data, 1024).to_vec();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk, [0, 1]);
    }

    #[test]
    fn split_zero_size_is_clamped_to_one() {
        let data: Bytes = vec![0u8; 4].into();
        let chunks = ChunkList::split(&data, 0).to_vec();
        assert_eq!(chunks.len(), 4);
        assert!(chunks.iter().all(|c| c.data.len() == 1));
    }

    #[test]
    fn split_chunks_share_one_message_id() {
        let data: Bytes = vec![0u8; 100].into();
        let chunks = ChunkList::split(&data, 32).to_vec();
        let id = chunks[0].meta.id;
        assert!(chunks.iter().all(|c| c.meta.id == id));
    }

    /// Cutting at any size and feeding the pieces back through the reassembler (in order) yields the
    /// original bytes — across exact multiples, remainders, single-chunk, and one-byte cuts.
    #[test]
    fn split_then_reassemble_round_trips() {
        for (len, size) in [
            (1usize, 7usize),
            (7, 7),
            (8, 7),
            (100, 7),
            (1000, 64),
            (5, 1),
        ] {
            let data: Bytes = (0..len).map(|i| i as u8).collect::<Vec<u8>>().into();
            let mut r = ChunkReassembler::new();
            let mut out = None;
            for c in ChunkList::split(&data, size) {
                out = r.handle(c).or(out);
            }
            assert_eq!(out.unwrap(), data, "len={len} size={size}");
        }
    }

    #[test]
    fn plan_whole_within_limit_inclusive() {
        assert_eq!(plan_framing(0, 100, 10), Framing::Whole);
        assert_eq!(plan_framing(50, 100, 10), Framing::Whole);
        // boundary: a payload exactly at the limit still goes whole.
        assert_eq!(plan_framing(100, 100, 10), Framing::Whole);
    }

    #[test]
    fn plan_chunks_just_over_limit() {
        assert_eq!(plan_framing(101, 100, 10), Framing::Chunked {
            chunk_size: 90
        });
    }

    /// The chunk size reserves the envelope, so `chunk_size + envelope_overhead ≤ limit`: a wrapped
    /// chunk can never exceed the negotiated limit.
    #[test]
    fn plan_chunk_size_reserves_envelope() {
        let (limit, overhead) = (65536usize, 4096usize);
        let Framing::Chunked { chunk_size } = plan_framing(limit * 2, limit, overhead) else {
            panic!("expected chunked");
        };
        assert_eq!(chunk_size, limit - overhead);
        assert!(chunk_size + overhead <= limit);
    }

    #[test]
    fn plan_chunk_size_floored_at_one() {
        // Degenerate: a limit at or below the envelope reserve still yields a positive cut.
        assert_eq!(plan_framing(100, 5, 10), Framing::Chunked { chunk_size: 1 });
        assert_eq!(plan_framing(100, 10, 10), Framing::Chunked {
            chunk_size: 1
        });
    }

    #[test]
    fn reassembles_in_order() {
        let data: Bytes = "helloworld".repeat(1024).into();
        let mut r = ChunkReassembler::new();
        let chunks = chunks_of(&data, 32);
        let mut out = None;
        for c in chunks {
            out = r.handle(c).or(out);
        }
        assert_eq!(out.unwrap(), data);
        assert_eq!(r.pending_count(), 0, "completed message is forgotten");
    }

    #[test]
    fn reassembles_out_of_order() {
        let data: Bytes = "helloworld".repeat(64).into();
        let mut chunks = chunks_of(&data, 32);
        chunks.reverse();
        let mut r = ChunkReassembler::new();
        let mut out = None;
        for c in chunks {
            out = r.handle(c).or(out);
        }
        assert_eq!(out.unwrap(), data);
    }

    #[test]
    fn duplicate_chunk_does_not_break_reassembly() {
        // Regression: arrival order [0, 1, 0] used to dedup-before-sort and never complete.
        let data: Bytes = "helloworld".repeat(8).into(); // > 32 bytes => 3 chunks
        let chunks = chunks_of(&data, 32);
        assert!(chunks.len() >= 2);
        let mut r = ChunkReassembler::new();

        // Feed every chunk, re-feeding chunk 0 in the middle as a duplicate.
        assert!(r.handle(chunks[0].clone()).is_none());
        for c in &chunks[1..] {
            let _ = r.handle(chunks[0].clone()); // duplicate of position 0, repeatedly
            if let Some(out) = r.handle(c.clone()) {
                assert_eq!(out, data);
                assert_eq!(r.pending_count(), 0);
                return;
            }
        }
        panic!("message never completed despite all chunks arriving");
    }

    #[test]
    fn interleaved_messages_are_isolated() {
        let d1: Bytes = "hello".repeat(64).into();
        let d2: Bytes = "world".repeat(64).into();
        let c1 = chunks_of(&d1, 32);
        let c2 = chunks_of(&d2, 32);
        let mut r = ChunkReassembler::new();

        // interleave the two messages
        let (mut o1, mut o2) = (None, None);
        for pair in c1.iter().zip(c2.iter()) {
            o1 = r.handle(pair.0.clone()).or(o1);
            o2 = r.handle(pair.1.clone()).or(o2);
        }
        // drain any tail (lengths may differ)
        for c in c1.iter().chain(c2.iter()) {
            let out = r.handle(c.clone());
            o1 = out.clone().filter(|b| *b == d1).or(o1);
            o2 = out.filter(|b| *b == d2).or(o2);
        }
        assert_eq!(o1.unwrap(), d1);
        assert_eq!(o2.unwrap(), d2);
    }

    #[test]
    fn incomplete_message_stays_pending() {
        let data: Bytes = "helloworld".repeat(64).into();
        let chunks = chunks_of(&data, 32);
        let mut r = ChunkReassembler::new();
        for c in &chunks[..chunks.len() - 1] {
            assert!(r.handle(c.clone()).is_none());
        }
        assert_eq!(r.pending_count(), 1);
        let out = r.handle(chunks.last().unwrap().clone());
        assert_eq!(out.unwrap(), data);
    }

    #[test]
    fn malformed_chunks_are_dropped() {
        let mut r = ChunkReassembler::new();
        // total == 0
        assert!(r
            .handle(Chunk {
                chunk: [0, 0],
                data: Bytes::from_static(b"x"),
                meta: ChunkMeta::default(),
            })
            .is_none());
        // position >= total
        assert!(r
            .handle(Chunk {
                chunk: [5, 3],
                data: Bytes::from_static(b"x"),
                meta: ChunkMeta::default(),
            })
            .is_none());
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn old_timestamp_does_not_panic() {
        // ts_ms < TS_OFFSET_TOLERANCE_MS would underflow a plain `u128` subtraction.
        let mut r = ChunkReassembler::new();
        let out = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"ok"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: 0,
                ttl_ms: DEFAULT_TTL_MS,
            },
        });
        assert_eq!(out.unwrap(), Bytes::from_static(b"ok"));
    }

    #[test]
    fn future_timestamp_is_dropped() {
        let mut r = ChunkReassembler::new();
        let out = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: get_epoch_ms() + 10 * TS_OFFSET_TOLERANCE_MS,
                ttl_ms: DEFAULT_TTL_MS,
            },
        });
        assert!(out.is_none());
    }

    #[test]
    fn expired_partial_messages_are_evicted() {
        let mut r = ChunkReassembler::new();
        let now = get_epoch_ms();
        // a partial (1 of 2) message that is already expired
        r.handle(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: now.saturating_sub(1000),
                ttl_ms: 100,
            },
        });
        // a fresh partial message triggers remove_expired, dropping the stale one
        r.handle(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"y"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: now,
                ttl_ms: DEFAULT_TTL_MS,
            },
        });
        assert_eq!(r.pending_count(), 1, "only the fresh partial remains");
    }

    #[test]
    fn pending_messages_are_capped() {
        let mut r = ChunkReassembler::new();
        // each is the first of two chunks => stays pending
        for _ in 0..(MAX_PENDING_MESSAGES + 10) {
            r.handle(Chunk {
                chunk: [0, 2],
                data: Bytes::from_static(b"x"),
                meta: ChunkMeta::default(), // fresh id, fresh ts each time
            });
        }
        assert_eq!(r.pending_count(), MAX_PENDING_MESSAGES);
    }

    #[test]
    fn round_trip_reordered_with_duplicates() {
        let data: Bytes = "abcdefghij".repeat(500).into();
        let mut chunks = chunks_of(&data, 64);
        // reorder + inject duplicates mid-stream (not after the final chunk, which would just
        // start a fresh, TTL-evicted pending entry — a late retransmit, not a reassembly bug).
        chunks.reverse();
        let dup = chunks[chunks.len() / 2].clone();
        chunks.insert(1, dup.clone());
        chunks.insert(chunks.len() / 3, dup);

        let mut r = ChunkReassembler::new();
        let mut out = None;
        for c in chunks {
            out = r.handle(c).or(out);
        }
        assert_eq!(out.unwrap(), data);
        assert_eq!(r.pending_count(), 0);
    }
}
