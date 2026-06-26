#![warn(missing_docs)]
//! Message framing / chunking. A message larger than the connection's negotiated
//! `max_message_size` is split into MTU-sized [`Chunk`]s on the sender and reassembled on the
//! receiver.
//!
//! NOTE: this is **whole-message** buffering, not MSRP-style (RFC 4975) streaming. There is no
//! mid-message interruption, interleaving, or incremental delivery — the receiver yields a payload
//! only once *every* chunk has arrived (or drops it on TTL). The "split into ordered, id-tagged
//! pieces and reassemble" idea is borrowed from MSRP chunking; the interruption semantics are not.
//!
//! Two halves, deliberately separated:
//!
//! - **Send** — [`ChunkList`] turns a [`Bytes`] into ordered [`Chunk`]s, where `chunk_size` comes
//!   from the connection's negotiated `max_message_size`. The sender uses [`ChunkList::stream`],
//!   which yields chunks lazily as zero-copy slices so one chunk is held in flight at a time;
//!   [`ChunkList::split`] (eager `Vec`) remains for tests.
//! - **Receive** — [`MessageReassembler`] collects incoming [`Chunk`]s keyed by message id and
//!   yields the original payload once every position has arrived.
//!
//! The receiver is robust to the realities of a multi-hop / DHT overlay: out-of-order arrival,
//! **duplicates / retransmits** (first write per position wins), and partial messages (evicted
//! by TTL). It is also bounded against a hostile peer: per-chunk and per-message byte caps, a
//! global buffered-cost ceiling (charging a per-slot overhead so tiny-chunk floods are bounded by
//! count too), an id-count cap, and up-front rejection of already-expired chunks. No single id and
//! no peer-supplied `total` can drive memory without limit. See [`MessageReassembler`].
//!
//! ```text
//!   send    : Bytes ↦ [Chunk{ chunk=[i, n], data=dataᵢ, meta } | i ∈ 0..n]   (Rust range, exclusive)
//!   receive : a message id is complete ⟺ received positions = 0..total (all n of them);
//!             then payload = concat(dataᵢ for i ∈ 0..total)
//! ```

use std::collections::btree_map::BTreeMap;
use std::collections::HashMap;

use bytes::Bytes;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
use crate::consts::MAX_TTL_MS;
use crate::consts::MIN_CHUNK_DATA;
use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::error::Error;
use crate::error::Result;
use crate::utils::get_epoch_ms;

/// The limits a [`MessageReassembler`] enforces on incoming chunks, as an explicit value rather
/// than module globals. This keeps the core admission rule independent of *where* the numbers come
/// from: the shell supplies them (see [`ReassemblyLimits::production`]), the reassembler only
/// enforces what it is given, and tests can use small limits instead of giant synthetic payloads.
#[derive(Debug, Clone, Copy)]
pub struct ReassemblyLimits {
    /// Max number of distinct in-flight message ids (a cheap first-line cap; the byte budgets are
    /// the real memory guard).
    pub max_pending_messages: usize,
    /// Max `data` bytes a single chunk may carry.
    pub max_chunk_data_len: usize,
    /// Max buffered data bytes for one in-flight message.
    pub max_message_bytes: usize,
    /// Max number of slots (chunks) one in-flight message may have — i.e. the largest `total` a
    /// chunk may claim. Caps the slot/`BTreeMap` count of a single message so a hostile peer cannot
    /// use one id with a huge `total` and tiny chunks to allocate millions of slots while staying
    /// under [`max_message_bytes`](Self::max_message_bytes) (which only counts data bytes).
    pub max_chunks_per_message: usize,
    /// Max buffered cost (data bytes + per-slot overhead) summed across all in-flight messages.
    pub max_total_buffered_cost: usize,
    /// Bookkeeping charge per slot — a *conservative estimate* (not an exact measurement) of the
    /// `BTreeMap` node plus `Bytes` header/refcount a slot costs, so a flood of *tiny* chunks is
    /// bounded by slot count, not only by summed data bytes. Real per-slot heap use may differ;
    /// this is deliberately generous so the budget over- rather than under-counts.
    pub slot_overhead: usize,
    /// Max number of recently-completed message ids remembered as tombstones, to suppress a
    /// re-delivery if a message is fully retransmitted after it already completed (within its TTL
    /// window). Bounds the tombstone memory. NOTE: past this many *concurrent* live tombstones the
    /// oldest is dropped even if its TTL has not elapsed, so the "no post-completion redelivery"
    /// guarantee holds only for the most recent `max_completed_ids` completions within a TTL window.
    pub max_completed_ids: usize,
}

impl ReassemblyLimits {
    /// The limits used in production, derived from the transport / message ceilings. This is the one
    /// place that reaches for transport-specific constants; the reassembler itself does not.
    pub fn production() -> Self {
        Self {
            max_pending_messages: 512,
            // A chunk crosses the wire as one data-channel message, capped by SCTP.
            max_chunk_data_len: MAX_DATA_CHANNEL_MESSAGE_SIZE,
            // The sender refuses to send more than this, so a larger reassembled message is forged;
            // this is what stops the "one id, huge `total`, stream unique positions" attack.
            max_message_bytes: TRANSPORT_MAX_SIZE,
            // The sender never produces chunks smaller than `MIN_CHUNK_DATA`, so a legitimate
            // message needs at most this many; a larger `total` is forged.
            max_chunks_per_message: TRANSPORT_MAX_SIZE / MIN_CHUNK_DATA + 1,
            // Admits several concurrent maximum-size transfers while staying hard-bounded.
            max_total_buffered_cost: TRANSPORT_MAX_SIZE * 4,
            slot_overhead: 128,
            max_completed_ids: 1024,
        }
    }

    /// Smaller limits for constrained deployments.
    ///
    /// This profile preserves the protocol-level 60 MB send ceiling elsewhere,
    /// but bounds one receiver's reassembly memory to a few MiB so weak devices
    /// can reject oversized in-flight transfers before allocating for them.
    pub fn constrained() -> Self {
        const CONSTRAINED_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
        const CONSTRAINED_TOTAL_COST: usize = 8 * 1024 * 1024;

        Self {
            max_pending_messages: 64,
            max_chunk_data_len: MAX_DATA_CHANNEL_MESSAGE_SIZE,
            max_message_bytes: CONSTRAINED_MESSAGE_BYTES,
            max_chunks_per_message: CONSTRAINED_MESSAGE_BYTES / MIN_CHUNK_DATA + 1,
            max_total_buffered_cost: CONSTRAINED_TOTAL_COST,
            slot_overhead: 128,
            max_completed_ids: 256,
        }
    }

    /// Clamp nonsensical values to safe minimums so a caller-supplied [`ReassemblyLimits`] cannot
    /// disable an invariant: every cap is forced to at least `1` (a `0` cap would, depending on the
    /// field, reject all traffic or — for `max_completed_ids` — silently void the tombstone
    /// guarantee the docs advertise). Applied by [`MessageReassembler::with_limits`].
    fn normalized(self) -> Self {
        Self {
            max_pending_messages: self.max_pending_messages.max(1),
            max_chunk_data_len: self.max_chunk_data_len.max(1),
            max_message_bytes: self.max_message_bytes.max(1),
            max_chunks_per_message: self.max_chunks_per_message.max(1),
            max_total_buffered_cost: self.max_total_buffered_cost.max(1),
            slot_overhead: self.slot_overhead,
            max_completed_ids: self.max_completed_ids.max(1),
        }
    }
}

impl Default for ReassemblyLimits {
    fn default() -> Self {
        Self::production()
    }
}

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
/// [`MessageReassembler`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChunkList(Vec<Chunk>);

impl ChunkList {
    /// Eagerly split `bytes` into chunks of at most `chunk_size` data bytes each, tagged
    /// `[i, total]`. A **test/helper** constructor (the production send path uses
    /// [`stream`](Self::stream), and [`WireReserves::plan`] never yields an unusable `chunk_size` —
    /// it returns `None` instead). `chunk_size` is clamped to ≥ 1 only as a defensive guard against
    /// a caller passing `0`; it is not a sanctioned way to produce 1-byte chunks on the wire.
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

    /// Stream `bytes` into chunks of at most `chunk_size` data bytes each **without materializing
    /// the whole list**: each chunk's `data` is a zero-copy [`Bytes::slice`] of the input, and the
    /// chunks are yielded lazily, so a sender can frame and flush one chunk at a time with bounded
    /// memory (rather than allocating every chunk up front). All chunks share one `[i, total]`
    /// numbering and one [`ChunkMeta`]. `chunk_size` is clamped to ≥ 1 so a degenerate value still
    /// terminates; empty input yields **no** chunks, agreeing with [`split`](Self::split).
    pub fn stream(bytes: Bytes, chunk_size: usize) -> impl Iterator<Item = Chunk> {
        let chunk_size = chunk_size.max(1);
        let total = bytes.len().div_ceil(chunk_size);
        let meta = ChunkMeta::default();
        (0..total).map(move |i| {
            let start = i * chunk_size;
            let end = start.saturating_add(chunk_size).min(bytes.len());
            Chunk {
                meta,
                chunk: [i, total],
                data: bytes.slice(start..end),
            }
        })
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

/// The bytes the transport adds around a payload on the wire, per framing path. Bundled as a named
/// value so the framing rule reads `reserves.plan(len, limit)` instead of a row of positional
/// `usize`s, and so the production reserves live in exactly one place ([`WireReserves::PRODUCTION`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireReserves {
    /// Bytes added around a *whole* payload — the outer `TransportMessage::Custom` frame.
    pub whole: usize,
    /// Bytes added around *each chunk's* data — its `MessagePayload` envelope **and** the outer
    /// `TransportMessage::Custom` frame.
    pub chunk: usize,
    /// Smallest per-chunk data payload worth producing; a limit that cannot fit `chunk +
    /// min_chunk_data` is rejected rather than fragmented into near-empty chunks.
    pub min_chunk_data: usize,
}

impl WireReserves {
    /// The reserves used in production, derived from the transport/message ceilings.
    pub const PRODUCTION: Self = Self {
        whole: TRANSPORT_CUSTOM_OVERHEAD,
        chunk: MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD,
        min_chunk_data: MIN_CHUNK_DATA,
    };

    /// Frame a `payload_len`-byte payload for a connection whose negotiated per-message limit is
    /// `max_message_size`. The decision is taken against the *wire* bytes (payload + reserves), not
    /// the bare payload, and is a pure total function:
    ///
    /// ```text
    ///   plan : (len, limit) ↦ Whole                  if len + whole ≤ limit
    ///                       ↦ Chunked(limit − chunk)  if limit ≥ chunk + min_chunk_data
    ///                       ↦ ∅                        otherwise
    /// ```
    ///
    /// `∅` (`None`) means the peer's limit is too small for even one useful chunk — a failure the
    /// caller surfaces, never a flood of 1-byte chunks. When `Chunked { chunk_size }` is returned,
    /// `min_chunk_data ≤ chunk_size` and `chunk_size + chunk ≤ limit`, so every wrapped chunk fits
    /// and a payload yields at most `⌈len / min_chunk_data⌉` chunks. Every sum is `checked`, so the
    /// function is total over all `usize` inputs (no overflow/underflow).
    pub fn plan(&self, payload_len: usize, max_message_size: usize) -> Option<Framing> {
        let whole_fits = payload_len
            .checked_add(self.whole)
            .is_some_and(|wire| wire <= max_message_size);
        if whole_fits {
            return Some(Framing::Whole);
        }
        let min_viable = self.chunk.checked_add(self.min_chunk_data)?;
        (max_message_size >= min_viable).then(|| Framing::Chunked {
            chunk_size: max_message_size - self.chunk,
        })
    }
}

/// One message being reassembled: the chunks seen so far, keyed by position.
struct Pending {
    /// total number of chunks the message claims (from `chunk[1]`).
    total: usize,
    /// received positions → bytes. A `BTreeMap` dedups by position (first write wins) and keeps
    /// the data ordered, so assembly is a single in-order concat.
    slots: BTreeMap<usize, Bytes>,
    /// running sum of buffered data bytes, so the per-message cap is O(1) to check.
    data_bytes: usize,
    /// creation time / ttl of the first chunk seen, used for TTL eviction.
    ts_ms: u128,
    ttl_ms: u64,
}

impl Pending {
    fn new(total: usize, ts_ms: u128, ttl_ms: u64) -> Self {
        Self {
            total,
            slots: BTreeMap::new(),
            data_bytes: 0,
            ts_ms,
            ttl_ms,
        }
    }

    /// Complete iff every position has arrived. Each inserted position is unique (map key) and in
    /// `0..total`, so `slots.len() == total` ⟺ the present set is exactly `{0..total-1}`.
    fn is_complete(&self) -> bool {
        self.slots.len() == self.total
    }

    /// Buffered cost charged to the global budget: data bytes plus `slot_overhead` per slot.
    /// Saturating arithmetic, so adversarial limit values can never overflow/wrap the budget —
    /// an overflowing cost simply saturates to `usize::MAX` and is rejected as over-budget.
    fn cost(&self, slot_overhead: usize) -> usize {
        self.slots
            .len()
            .saturating_mul(slot_overhead)
            .saturating_add(self.data_bytes)
    }

    fn assemble(self) -> Bytes {
        self.slots.into_values().flatten().collect()
    }
}

/// Receiver side: **whole-message** reassembly for reliable data-channel `MessagePayload`
/// fragments. Buffers a message's chunks keyed by id and yields the complete [`Bytes`] once every
/// position has arrived (then forgets it).
///
/// Correct under duplicates / retransmits (first write per position wins *during* assembly,
/// out-of-order arrival sorted), partial delivery (TTL eviction), and a message **fully
/// retransmitted after it already completed**: a completed id is kept as a tombstone until it would
/// expire, so a late re-send within the TTL window is dropped rather than re-assembled and delivered
/// twice.
///
/// **Bounded against a hostile peer** by the [`ReassemblyLimits`] it is built with: every accepted
/// chunk is validated and charged to a budget, so reassembly memory cannot grow without limit no
/// matter how the load is shaped — per-chunk data, per-message data, a global buffered-cost ceiling
/// (charging a per-slot overhead so a tiny-chunk flood is bounded by slot count too), the id count,
/// and the completed-id tombstone set are all capped, and an already-expired chunk is rejected
/// before it can be delivered or buffered.
pub struct MessageReassembler {
    pending: HashMap<Uuid, Pending>,
    /// Sum of `Pending::cost(..)` over `pending`, maintained incrementally for an O(1) global cap.
    buffered_cost: usize,
    /// Tombstones for ids that have already been delivered, each paired with its expiry
    /// (`ts_ms + ttl_ms`). A chunk for one of these is dropped, so a post-completion retransmit of a
    /// whole message is not re-assembled and delivered again. `VecDeque` for FIFO/TTL eviction, the
    /// `HashSet` for an O(1) membership check; the two are kept in lockstep.
    completed: std::collections::VecDeque<(Uuid, u128)>,
    completed_ids: std::collections::HashSet<Uuid>,
    /// The bounds enforced on every incoming chunk.
    limits: ReassemblyLimits,
}

impl Default for MessageReassembler {
    fn default() -> Self {
        Self::with_limits(ReassemblyLimits::production())
    }
}

impl MessageReassembler {
    /// Empty reassembler with [`ReassemblyLimits::production`] bounds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty reassembler enforcing the given `limits`. Tests use this with small limits to exercise
    /// the admission rule without giant synthetic payloads.
    pub fn with_limits(limits: ReassemblyLimits) -> Self {
        Self {
            pending: HashMap::new(),
            buffered_cost: 0,
            completed: std::collections::VecDeque::new(),
            completed_ids: std::collections::HashSet::new(),
            // Clamp nonsensical caps so a caller cannot disable an invariant (e.g. a `0` cap).
            limits: limits.normalized(),
        }
    }

    /// Record `id` as delivered so a later full retransmit (within the TTL window) is suppressed,
    /// dropping the oldest tombstone if the cap is reached. `expiry` is the message's `ts_ms + ttl_ms`
    /// — after it, a retransmit is rejected by the expiry check anyway, so the tombstone can go.
    fn mark_completed(&mut self, id: Uuid, expiry: u128) {
        if self.completed_ids.insert(id) {
            self.completed.push_back((id, expiry));
        }
        while self.completed.len() > self.limits.max_completed_ids {
            if let Some((old, _)) = self.completed.pop_front() {
                self.completed_ids.remove(&old);
            }
        }
    }

    /// Number of messages currently being reassembled (incomplete).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Drop messages whose TTL has elapsed, returning their cost to the budget, and evict completed-id
    /// tombstones that have likewise expired (a retransmit past its expiry is rejected anyway).
    pub fn remove_expired(&mut self) {
        self.remove_expired_at(get_epoch_ms());
    }

    /// [`remove_expired`](Self::remove_expired) with the clock injected (tests pass a controlled
    /// `now` to drive the real eviction logic).
    fn remove_expired_at(&mut self, now: u128) {
        let buffered_cost = &mut self.buffered_cost;
        let slot_overhead = self.limits.slot_overhead;
        self.pending.retain(|_, p| {
            let alive = p.ts_ms.saturating_add(p.ttl_ms as u128) > now;
            if !alive {
                *buffered_cost = buffered_cost.saturating_sub(p.cost(slot_overhead));
            }
            alive
        });
        // Evict *every* expired tombstone, not just a leading run: completion order need not equal
        // expiry order, so a `retain` is correct where front-popping would leave an out-of-order
        // early-expiry entry behind a still-live front.
        let completed_ids = &mut self.completed_ids;
        self.completed.retain(|&(id, expiry)| {
            let alive = expiry > now;
            if !alive {
                completed_ids.remove(&id);
            }
            alive
        });
    }

    /// Forget a message (e.g. after it has been delivered), returning its cost to the budget.
    pub fn remove(&mut self, id: Uuid) {
        if let Some(p) = self.pending.remove(&id) {
            self.buffered_cost -= p.cost(self.limits.slot_overhead);
        }
    }

    /// Accept one chunk. Returns the fully reassembled payload when this chunk completes its
    /// message (which is then forgotten), otherwise `None`.
    ///
    /// Imperative shell over a functional core: expire stale state, ask the pure `classify` for an
    /// admission verdict, and apply it. The only mutation of the buffer is in `admit`; a rejected
    /// chunk leaves no trace and is logged once with its typed `Rejected` reason.
    pub fn handle(&mut self, chunk: Chunk) -> Option<Bytes> {
        self.handle_at(chunk, get_epoch_ms())
    }

    /// [`handle`](Self::handle) with the clock injected, so tests drive expiry/admission against a
    /// controlled `now` through the real production path instead of poking internal state.
    fn handle_at(&mut self, chunk: Chunk, now: u128) -> Option<Bytes> {
        // Reclaim expired pending entries and tombstones FIRST — before classify reads them — so
        // invalid traffic still frees memory and an expired tombstone cannot suppress a fresh
        // message that reuses its id after the TTL window.
        self.remove_expired_at(now);
        match self.classify(&chunk, now) {
            Ok(cost) => self.admit(chunk, cost),
            Err(reason) => {
                tracing::debug!(?reason, id = ?chunk.meta.id, "reassembler dropped chunk");
                None
            }
        }
    }

    /// The pure admission rule: `(state, chunk, now) ↦ Ok(cost) | Err(reason)`. Borrows `&self`,
    /// mutates nothing, does no I/O. On success it returns the buffered cost [`admit`] must charge;
    /// on failure a typed [`Rejected`] reason. Validating the existing pending entry here, before
    /// any mutation, is what keeps a rejected chunk side-effect-free and the accounting exact.
    ///
    /// [`admit`]: Self::admit
    fn classify(&self, chunk: &Chunk, now: u128) -> std::result::Result<usize, Rejected> {
        let meta = &chunk.meta;
        if meta.ttl_ms > MAX_TTL_MS {
            return Err(Rejected::TtlTooLarge);
        }
        // `saturating_sub` avoids the `u128` underflow a forged `ts_ms < TS_OFFSET_TOLERANCE_MS`
        // would cause; `saturating_add` avoids overflow on a forged ttl.
        if meta.ts_ms.saturating_sub(TS_OFFSET_TOLERANCE_MS) > now {
            return Err(Rejected::FutureTimestamp);
        }
        // Reject an already-expired chunk up front, so a stale `total == 1` is never delivered.
        if meta.ts_ms.saturating_add(meta.ttl_ms as u128) <= now {
            return Err(Rejected::Expired);
        }

        let [position, total] = chunk.chunk;
        // A real message has ≥ 1 chunk and every position in `0..total`.
        if total == 0 || position >= total {
            return Err(Rejected::Malformed);
        }
        // Cap the slot count: a forged `total` is refused before it can allocate a huge `BTreeMap`.
        if total > self.limits.max_chunks_per_message {
            return Err(Rejected::TooManyChunks);
        }
        // One chunk cannot exceed one data-channel message.
        if chunk.data.len() > self.limits.max_chunk_data_len {
            return Err(Rejected::ChunkTooLarge);
        }
        // Already delivered: drop a post-completion retransmit (expired tombstones were swept).
        if self.completed_ids.contains(&meta.id) {
            return Err(Rejected::AlreadyCompleted);
        }

        // Bytes already buffered for this id (`0` for a new message). Used for the per-message cap
        // below, which must hold for the *first* chunk too — not only once a pending entry exists —
        // or a caller-supplied `max_chunk_data_len > max_message_bytes` could admit an oversized
        // lone chunk.
        let buffered_for_id = match self.pending.get(&meta.id) {
            // A new id: admit only if there is room for another concurrent message.
            None if self.pending.len() >= self.limits.max_pending_messages => {
                return Err(Rejected::PendingFull);
            }
            None => 0,
            Some(p) => {
                // A chunk of an in-flight message must agree on its shape and provenance.
                if p.total != total {
                    return Err(Rejected::TotalMismatch);
                }
                // Chunks of one message share id+ts+ttl; a same-id chunk from a different
                // transmission must not be merged in (it would skew expiry/tombstone behaviour).
                if p.ts_ms != meta.ts_ms || p.ttl_ms != meta.ttl_ms {
                    return Err(Rejected::MetadataMismatch);
                }
                // First write per position wins; a duplicate position is a no-op, not an error.
                if p.slots.contains_key(&position) {
                    return Err(Rejected::DuplicatePosition);
                }
                p.data_bytes
            }
        };
        // Per-message data cap, enforced uniformly across the first and subsequent chunks.
        if buffered_for_id.saturating_add(chunk.data.len()) > self.limits.max_message_bytes {
            return Err(Rejected::PerMessageBytes);
        }

        // Cost charged to the global budget: this slot's data + its fixed overhead. Saturating, so a
        // pathological `slot_overhead` cannot wrap the budget.
        let cost = chunk.data.len().saturating_add(self.limits.slot_overhead);
        if self.buffered_cost.saturating_add(cost) > self.limits.max_total_buffered_cost {
            return Err(Rejected::GlobalBudget);
        }
        Ok(cost)
    }

    /// The sole buffer mutation: insert a [`classify`]-approved `chunk` (charging `cost`), and if it
    /// completes its message, take it out, refund its budget, tombstone the id, and return the
    /// reassembled payload.
    ///
    /// [`classify`]: Self::classify
    fn admit(&mut self, chunk: Chunk, cost: usize) -> Option<Bytes> {
        let id = chunk.meta.id;
        let [position, _total] = chunk.chunk;
        let pending = self
            .pending
            .entry(id)
            .or_insert_with(|| Pending::new(chunk.chunk[1], chunk.meta.ts_ms, chunk.meta.ttl_ms));
        pending.data_bytes = pending.data_bytes.saturating_add(chunk.data.len());
        pending.slots.insert(position, chunk.data);
        self.buffered_cost = self.buffered_cost.saturating_add(cost);

        if !pending.is_complete() {
            return None;
        }
        let done = self.pending.remove(&id)?;
        self.buffered_cost = self
            .buffered_cost
            .saturating_sub(done.cost(self.limits.slot_overhead));
        // Tombstone the id until it would expire, so a later full retransmit is suppressed.
        self.mark_completed(id, done.ts_ms.saturating_add(done.ttl_ms as u128));
        Some(done.assemble())
    }
}

/// Why a chunk was not admitted — a *value*, so [`MessageReassembler::classify`] stays a pure total
/// function the shell can test and log uniformly, rather than scattering ad-hoc log strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejected {
    /// `ttl_ms` exceeds [`MAX_TTL_MS`].
    TtlTooLarge,
    /// Stamped further in the future than [`TS_OFFSET_TOLERANCE_MS`] allows.
    FutureTimestamp,
    /// Already past its `ts_ms + ttl_ms` expiry.
    Expired,
    /// `total == 0` or `position >= total`.
    Malformed,
    /// `total` exceeds [`ReassemblyLimits::max_chunks_per_message`].
    TooManyChunks,
    /// `data` exceeds [`ReassemblyLimits::max_chunk_data_len`].
    ChunkTooLarge,
    /// The message id is tombstoned (already delivered).
    AlreadyCompleted,
    /// A new id, but [`ReassemblyLimits::max_pending_messages`] is already reached.
    PendingFull,
    /// `total` disagrees with the in-flight message's.
    TotalMismatch,
    /// `ts_ms`/`ttl_ms` disagree with the in-flight message's (a different transmission).
    MetadataMismatch,
    /// This position is already buffered (a duplicate/retransmit).
    DuplicatePosition,
    /// Admitting would exceed the message's [`ReassemblyLimits::max_message_bytes`].
    PerMessageBytes,
    /// Admitting would exceed the global [`ReassemblyLimits::max_total_buffered_cost`].
    GlobalBudget,
}

#[cfg(test)]
mod test {
    use super::*;

    fn chunks_of(data: &Bytes, mtu: usize) -> Vec<Chunk> {
        ChunkList::split(data, mtu).into()
    }

    /// Tiny limits so the admission rule can be exercised without giant synthetic payloads.
    fn small_limits() -> ReassemblyLimits {
        ReassemblyLimits {
            max_pending_messages: 4,
            max_chunk_data_len: 16,
            max_message_bytes: 100,
            max_chunks_per_message: 64,
            max_total_buffered_cost: 256,
            slot_overhead: 8,
            max_completed_ids: 8,
        }
    }

    #[test]
    fn constrained_reassembly_limits_are_smaller_than_production() {
        let production = ReassemblyLimits::production();
        let constrained = ReassemblyLimits::constrained();

        assert!(constrained.max_pending_messages < production.max_pending_messages);
        assert!(constrained.max_message_bytes < production.max_message_bytes);
        assert!(constrained.max_chunks_per_message < production.max_chunks_per_message);
        assert!(constrained.max_total_buffered_cost < production.max_total_buffered_cost);
        assert!(constrained.max_completed_ids < production.max_completed_ids);
        assert_eq!(
            constrained.max_chunk_data_len,
            production.max_chunk_data_len
        );
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
            let mut r = MessageReassembler::new();
            let mut out = None;
            for c in ChunkList::split(&data, size) {
                out = r.handle(c).or(out);
            }
            assert_eq!(out.unwrap(), data, "len={len} size={size}");
        }
    }

    /// Test reserves with readable, distinct values (`whole < chunk`) so the two paths are easy to
    /// tell apart in the assertions below.
    fn reserves(whole: usize, chunk: usize, min_chunk_data: usize) -> WireReserves {
        WireReserves {
            whole,
            chunk,
            min_chunk_data,
        }
    }

    #[test]
    fn plan_whole_includes_whole_overhead() {
        let r = reserves(10, 20, 1);
        // Whole fits while payload + whole ≤ limit, up to and including the boundary.
        assert_eq!(r.plan(0, 100), Some(Framing::Whole));
        assert_eq!(r.plan(90, 100), Some(Framing::Whole));
        // One past the boundary must chunk.
        assert_eq!(r.plan(91, 100), Some(Framing::Chunked { chunk_size: 80 }));
    }

    /// The chunk size reserves the chunk overhead, so `chunk_size + chunk ≤ limit`: a wrapped chunk
    /// can never exceed the negotiated limit.
    #[test]
    fn plan_chunk_size_reserves_overhead() {
        let (limit, chunk_overhead) = (65536usize, 4096usize);
        let Some(Framing::Chunked { chunk_size }) =
            reserves(16, chunk_overhead, 16).plan(limit * 2, limit)
        else {
            panic!("expected chunked");
        };
        assert_eq!(chunk_size, limit - chunk_overhead);
        assert!(chunk_size + chunk_overhead <= limit);
    }

    #[test]
    fn plan_none_when_chunk_too_small() {
        // A limit that cannot fit `chunk + min_chunk_data` is rejected outright, not split tiny.
        assert_eq!(reserves(4, 10, 1).plan(100, 5), None); // below the overhead
        assert_eq!(reserves(4, 10, 1).plan(100, 10), None); // == overhead, 0 data bytes
                                                            // limit just clears chunk + min: the smallest *allowed* cut.
        assert_eq!(
            reserves(4, 10, 1).plan(100, 11),
            Some(Framing::Chunked { chunk_size: 1 })
        );
        // a realistic floor: min_chunk_data = 8 needs limit ≥ chunk + 8.
        assert_eq!(reserves(4, 10, 8).plan(100, 17), None); // 17 < 10 + 8
        assert_eq!(
            reserves(4, 10, 8).plan(100, 18),
            Some(Framing::Chunked { chunk_size: 8 })
        );
    }

    #[test]
    fn plan_is_total_on_overflow() {
        // `payload_len + whole` overflows usize; must not panic, and (not a whole fit) falls through
        // to the chunked decision rather than wrapping around.
        assert_eq!(
            reserves(10, 20, 1).plan(usize::MAX, 100),
            Some(Framing::Chunked { chunk_size: 80 })
        );
        // overflow with a too-small limit still yields None, not a panic.
        assert_eq!(reserves(10, 20, 1).plan(usize::MAX, 10), None);
    }

    #[test]
    fn reassembles_in_order() {
        let data: Bytes = "helloworld".repeat(1024).into();
        let mut r = MessageReassembler::new();
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
        let mut r = MessageReassembler::new();
        let mut out = None;
        for c in chunks {
            out = r.handle(c).or(out);
        }
        assert_eq!(out.unwrap(), data);
    }

    #[test]
    fn full_retransmit_after_completion_is_not_redelivered() {
        // A message that completes, then is *fully* retransmitted within its TTL window, must not be
        // delivered a second time — the completed id is tombstoned.
        let data: Bytes = "helloworld".repeat(64).into();
        let chunks = chunks_of(&data, 32);
        assert!(chunks.len() > 1, "need a multi-chunk message for this test");

        let mut r = MessageReassembler::new();
        let mut first = None;
        for c in chunks.clone() {
            first = r.handle(c).or(first);
        }
        assert_eq!(first.unwrap(), data, "first assembly delivers once");
        assert_eq!(r.pending_count(), 0);

        // Replay every chunk of the same message; none should re-open a pending entry or re-deliver.
        for c in chunks {
            assert!(
                r.handle(c).is_none(),
                "a retransmit of an already-completed message must be dropped"
            );
        }
        assert_eq!(
            r.pending_count(),
            0,
            "no pending re-opened by the retransmit"
        );
    }

    #[test]
    fn duplicate_chunk_does_not_break_reassembly() {
        // Regression: arrival order [0, 1, 0] used to dedup-before-sort and never complete.
        let data: Bytes = "helloworld".repeat(8).into(); // > 32 bytes => 3 chunks
        let chunks = chunks_of(&data, 32);
        assert!(chunks.len() >= 2);
        let mut r = MessageReassembler::new();

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
        let mut r = MessageReassembler::new();

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
        let mut r = MessageReassembler::new();
        for c in &chunks[..chunks.len() - 1] {
            assert!(r.handle(c.clone()).is_none());
        }
        assert_eq!(r.pending_count(), 1);
        let out = r.handle(chunks.last().unwrap().clone());
        assert_eq!(out.unwrap(), data);
    }

    #[test]
    fn malformed_chunks_are_dropped() {
        let mut r = MessageReassembler::new();
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
    fn old_timestamp_is_dropped_without_panic() {
        // ts_ms < TS_OFFSET_TOLERANCE_MS would underflow a plain `u128` subtraction (no panic with
        // saturating arithmetic), and a chunk stamped at the epoch is already long expired — it must
        // be dropped, not delivered, even though it is a complete `total == 1` message.
        let mut r = MessageReassembler::new();
        let out = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"ok"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: 0,
                ttl_ms: DEFAULT_TTL_MS,
            },
        });
        assert!(out.is_none());
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn expired_single_chunk_is_not_delivered() {
        // Regression: sweeping *other* pending entries before insertion let an already-expired
        // `total == 1` chunk be delivered immediately. It must be rejected up front.
        let mut r = MessageReassembler::new();
        let now = get_epoch_ms();
        let out = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: now.saturating_sub(1000),
                ttl_ms: 100, // expired 900ms ago
            },
        });
        assert!(out.is_none());
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn oversize_chunk_data_is_rejected() {
        let limits = small_limits();
        let mut r = MessageReassembler::with_limits(limits);
        let data: Bytes = vec![0u8; limits.max_chunk_data_len + 1].into();
        let out = r.handle(Chunk {
            chunk: [0, 1],
            data,
            meta: ChunkMeta::default(),
        });
        assert!(out.is_none());
        assert_eq!(r.pending_count(), 0);
        assert_eq!(r.buffered_cost, 0);
    }

    #[test]
    fn buffered_cost_returns_to_zero_after_completion() {
        let data: Bytes = "helloworld".repeat(100).into();
        let mut r = MessageReassembler::new();
        for c in ChunkList::split(&data, 32) {
            r.handle(c);
        }
        assert_eq!(r.pending_count(), 0);
        assert_eq!(r.buffered_cost, 0, "completing a message frees its budget");
    }

    /// A single id advertising a huge `total` and streaming distinct positions cannot grow without
    /// bound: the per-message byte cap stops it.
    #[test]
    fn per_message_byte_cap_bounds_one_id() {
        let limits = small_limits();
        let mut r = MessageReassembler::with_limits(limits);
        let meta = ChunkMeta::default();
        let data: Bytes = vec![0u8; limits.max_chunk_data_len].into();
        // Within the slot cap, but its data far exceeds the per-message byte cap so it never fills.
        let total = limits.max_chunks_per_message;

        let mut accepted = 0usize;
        for position in 0..50 {
            let before = r.pending.get(&meta.id).map(|p| p.slots.len()).unwrap_or(0);
            r.handle(Chunk {
                meta,
                chunk: [position, total],
                data: data.clone(),
            });
            let after = r.pending.get(&meta.id).map(|p| p.slots.len()).unwrap_or(0);
            if after > before {
                accepted += 1;
            }
        }

        let pending = r.pending.get(&meta.id).expect("still pending");
        assert!(
            pending.data_bytes <= limits.max_message_bytes,
            "per-message buffered data must stay within the cap"
        );
        assert!(
            accepted < 50,
            "the cap must reject some chunks, got {accepted}"
        );
        assert_eq!(
            r.buffered_cost,
            pending.cost(limits.slot_overhead),
            "accounting stays exact"
        );
    }

    /// Spreading the flood across many ids is bounded too: the global buffered-cost ceiling caps
    /// total memory regardless of how many ids are used.
    #[test]
    fn global_cost_cap_bounds_total() {
        let limits = small_limits();
        let mut r = MessageReassembler::with_limits(limits);
        // Each id contributes one slot of `max_chunk_data_len` data; keep them all pending.
        for _ in 0..(limits.max_pending_messages * 4) {
            r.handle(Chunk {
                chunk: [0, 2],
                data: vec![0u8; limits.max_chunk_data_len].into(),
                meta: ChunkMeta::default(),
            });
        }
        assert!(
            r.buffered_cost <= limits.max_total_buffered_cost,
            "global buffered cost {} exceeded cap {}",
            r.buffered_cost,
            limits.max_total_buffered_cost
        );
    }

    #[test]
    fn future_timestamp_is_dropped() {
        let mut r = MessageReassembler::new();
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
        let mut r = MessageReassembler::new();
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
        let limits = small_limits();
        let mut r = MessageReassembler::with_limits(limits);
        // each is the first of two chunks => stays pending
        for _ in 0..(limits.max_pending_messages + 10) {
            r.handle(Chunk {
                chunk: [0, 2],
                data: Bytes::from_static(b"x"),
                meta: ChunkMeta::default(), // fresh id, fresh ts each time
            });
        }
        assert_eq!(r.pending_count(), limits.max_pending_messages);
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

        let mut r = MessageReassembler::new();
        let mut out = None;
        for c in chunks {
            out = r.handle(c).or(out);
        }
        assert_eq!(out.unwrap(), data);
        assert_eq!(r.pending_count(), 0);
    }

    /// A forged `total` larger than the per-message slot cap is rejected before it can allocate a
    /// huge slot map, even though each individual chunk's data is tiny.
    #[test]
    fn total_over_slot_cap_is_rejected() {
        let limits = small_limits();
        let mut r = MessageReassembler::with_limits(limits);
        let out = r.handle(Chunk {
            chunk: [0, limits.max_chunks_per_message + 1],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta::default(),
        });
        assert!(out.is_none());
        assert_eq!(r.pending_count(), 0);
        assert_eq!(r.buffered_cost, 0);
    }

    /// Two chunks sharing an id/total but from different transmissions (different `ts_ms`/`ttl_ms`)
    /// must not be merged into one pending entry.
    #[test]
    fn mismatched_ts_or_ttl_for_same_id_is_rejected() {
        let mut r = MessageReassembler::new();
        let id = Uuid::new_v4();
        let now = get_epoch_ms();
        assert!(r
            .handle(Chunk {
                chunk: [0, 2],
                data: Bytes::from_static(b"a"),
                meta: ChunkMeta {
                    id,
                    ts_ms: now,
                    ttl_ms: DEFAULT_TTL_MS
                },
            })
            .is_none());
        // Same id/total, different ts_ms → rejected (a chunk from another transmission).
        let out = r.handle(Chunk {
            chunk: [1, 2],
            data: Bytes::from_static(b"b"),
            meta: ChunkMeta {
                id,
                ts_ms: now + 1,
                ttl_ms: DEFAULT_TTL_MS,
            },
        });
        assert!(out.is_none(), "must not complete by mixing transmissions");
        let p = r.pending.get(&id).expect("first chunk still pending");
        assert_eq!(p.slots.len(), 1, "the mismatched chunk left no trace");
    }

    /// Once a completed message's TTL elapses, its tombstone is evicted by the real
    /// `remove_expired_at` path (driven here by an injected clock, not by poking internal state),
    /// and a fresh message reusing the same id is then accepted rather than suppressed.
    #[test]
    fn tombstone_expires_then_id_is_reusable() {
        let mut r = MessageReassembler::new();
        let id = Uuid::new_v4();
        // A fixed base well above the future-skew tolerance, so timestamps are unambiguous.
        let base = 1_000_000u128;
        let ttl = 100u64;
        let one_chunk = |label: &'static [u8], ts_ms: u128, ttl_ms: u64| Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(label),
            meta: ChunkMeta { id, ts_ms, ttl_ms },
        };

        // Complete a 1-chunk message at t = base; its tombstone expires at base + ttl.
        let first = r.handle_at(one_chunk(b"first", base, ttl), base);
        assert_eq!(first.as_deref(), Some(&b"first"[..]));
        assert!(r.completed_ids.contains(&id), "tombstoned after completion");

        // A full retransmit *within* the TTL window (t = base + ttl/2) is suppressed.
        let dup = r.handle_at(one_chunk(b"first", base, ttl), base + (ttl as u128) / 2);
        assert!(
            dup.is_none(),
            "post-completion retransmit suppressed within TTL"
        );
        assert!(
            r.completed_ids.contains(&id),
            "tombstone still live within TTL"
        );

        // Past the tombstone's expiry (t = base + ttl + 1), a brand-new message reusing the id is
        // delivered: `remove_expired_at` evicts the now-expired tombstone before classify runs.
        let later = base + ttl as u128 + 1;
        let reused = r.handle_at(one_chunk(b"second", later, ttl), later);
        assert_eq!(
            reused.as_deref(),
            Some(&b"second"[..]),
            "id reusable after its tombstone expired via remove_expired_at"
        );
    }
}
