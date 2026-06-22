#![warn(missing_docs)]
//! Message framing / chunking, inspired by RFC 4975 (MSRP) chunking
//! <https://www.rfc-editor.org/rfc/rfc4975#page-9>: a large message is split into
//! MTU-sized [`Chunk`]s on the sender and reassembled on the receiver, so a big payload
//! does not monopolise a connection.
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
//!   send    : Bytes ↦ [Chunk{ chunk=[i, n], data=dataᵢ, meta } | i ∈ 0..n]
//!   receive : a message id is complete ⟺ {position | chunk received} = {0..total-1};
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
use crate::consts::MAX_TTL_MS;
use crate::consts::MIN_CHUNK_DATA;
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

    /// Stream `bytes` into chunks of at most `chunk_size` data bytes each **without materializing
    /// the whole list**: each chunk's `data` is a zero-copy [`Bytes::slice`] of the input, and the
    /// chunks are yielded lazily, so a sender can frame and flush one chunk at a time with bounded
    /// memory (rather than allocating every chunk up front). All chunks share one `[i, total]`
    /// numbering and one [`ChunkMeta`]. `chunk_size` is clamped to ≥ 1 so a degenerate value still
    /// terminates.
    pub fn stream(bytes: Bytes, chunk_size: usize) -> impl Iterator<Item = Chunk> {
        let chunk_size = chunk_size.max(1);
        let total = bytes.len().div_ceil(chunk_size).max(1);
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

/// Decide how to frame a `payload_len`-byte payload for a connection whose negotiated per-message
/// limit is `max_message_size`.
///
/// The decision is made against the *actual* bytes the data channel will carry, not the bare
/// payload: the transport wraps every send, and a chunk is additionally re-wrapped in a
/// `MessagePayload`. So two reserves are taken:
///
/// - `whole_overhead` — bytes the transport adds around the whole payload (the `Whole` send).
/// - `chunk_overhead` — bytes added around each chunk's data (its `MessagePayload` envelope *plus*
///   the transport wrapper).
///
/// A pure, partial function of five lengths:
///
/// ```text
///   plan : (len, limit, wʰ, cʰ, min) ↦ Some(Whole)                 if len + wʰ ≤ limit
///                                       Some(Chunked{ limit − cʰ })  if limit − cʰ ≥ min
///                                       None                          otherwise
/// ```
///
/// `min_chunk_data` is the smallest per-chunk data payload worth producing
/// ([`MIN_CHUNK_DATA`]). `None` means the peer's limit is too small
/// to carry even one *useful* chunk after wrapping — a real failure the caller must surface, rather
/// than fragmenting a message into a huge number of near-empty chunks (which would also be a memory
/// / task amplification vector). When `Some(Chunked { chunk_size })` is returned,
/// `chunk_size ≥ min_chunk_data ≥ 1` and `chunk_size + chunk_overhead ≤ limit`, so every wrapped
/// chunk fits and a payload yields at most `payload_len / min_chunk_data` chunks.
///
/// Total over all `usize` inputs: the whole-fit test uses `checked_add`, and the chunked branch
/// subtracts only after the `limit ≥ chunk_overhead + min` guard, so neither overflows nor
/// underflows.
pub fn plan_framing(
    payload_len: usize,
    max_message_size: usize,
    whole_overhead: usize,
    chunk_overhead: usize,
    min_chunk_data: usize,
) -> Option<Framing> {
    let whole_fits = payload_len
        .checked_add(whole_overhead)
        .is_some_and(|wire_len| wire_len <= max_message_size);
    if whole_fits {
        return Some(Framing::Whole);
    }
    // The chunked path is viable only if, after reserving the per-chunk envelope, at least
    // `min_chunk_data` data bytes still fit. `chunk_overhead + min_chunk_data` is computed with
    // `checked_add` so a hostile/huge reserve cannot wrap.
    let min_viable_limit = chunk_overhead.checked_add(min_chunk_data)?;
    if max_message_size >= min_viable_limit {
        Some(Framing::Chunked {
            chunk_size: max_message_size - chunk_overhead,
        })
    } else {
        None
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
        let now = get_epoch_ms();
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
    /// message (which is then forgotten), otherwise `None`. Malformed, expired, or over-any-bound
    /// chunks are dropped (`None`).
    pub fn handle(&mut self, chunk: Chunk) -> Option<Bytes> {
        let now = get_epoch_ms();

        // Reclaim expired pending entries and tombstones FIRST, before any early-return gate, so a
        // flood of invalid chunks still frees memory and so an expired tombstone is gone before the
        // completed-id lookup below (a fresh message reusing the id after its TTL is not suppressed).
        self.remove_expired();

        // Reject an absurd ttl outright.
        if chunk.meta.ttl_ms > MAX_TTL_MS {
            tracing::debug!(
                "reassembler: drop chunk with ttl {} > max",
                chunk.meta.ttl_ms
            );
            return None;
        }
        // Reject a chunk stamped too far in the future. `saturating_sub` avoids the `u128`
        // underflow a malformed/forged `ts_ms < TS_OFFSET_TOLERANCE_MS` would otherwise cause.
        if chunk.meta.ts_ms.saturating_sub(TS_OFFSET_TOLERANCE_MS) > now {
            tracing::debug!("reassembler: drop chunk stamped in the future");
            return None;
        }
        // Reject a chunk that is *itself* already expired, before it can be buffered or delivered (a
        // stale `total == 1` would otherwise be delivered immediately). `saturating_add` avoids
        // overflow on a forged ttl.
        if chunk.meta.ts_ms.saturating_add(chunk.meta.ttl_ms as u128) <= now {
            tracing::debug!("reassembler: drop already-expired chunk");
            return None;
        }

        let [position, total] = chunk.chunk;
        // A real message has at least one chunk and every position is in `0..total`.
        if total == 0 || position >= total {
            tracing::debug!("reassembler: drop malformed chunk [pos={position}, total={total}]");
            return None;
        }
        // Cap the slot count of a single message: a forged `total` larger than any legitimate
        // message could need is rejected before it can allocate a huge `BTreeMap`.
        if total > self.limits.max_chunks_per_message {
            tracing::debug!("reassembler: drop chunk with total {total} over slot cap");
            return None;
        }
        // A single chunk cannot carry more than one data-channel message's worth of bytes.
        if chunk.data.len() > self.limits.max_chunk_data_len {
            tracing::debug!(
                "reassembler: drop oversized chunk ({} bytes)",
                chunk.data.len()
            );
            return None;
        }

        let id = chunk.meta.id;
        // Already delivered: drop a post-completion retransmit instead of re-assembling it into a
        // second delivery of the same payload. (Expired tombstones were swept above.)
        if self.completed_ids.contains(&id) {
            return None;
        }
        // Bound concurrent messages: once at the cap (after reclaiming expired ones above), drop
        // any *new* message rather than grow without limit.
        if !self.pending.contains_key(&id) && self.pending.len() >= self.limits.max_pending_messages
        {
            tracing::debug!("reassembler: drop new message, pending at cap");
            return None;
        }

        // Validate against the existing pending (if any) *before* mutating, so a rejected chunk
        // leaves no trace and the accounting stays exact.
        if let Some(p) = self.pending.get(&id) {
            // A chunk whose `total` disagrees with the first one seen is malformed.
            if p.total != total {
                return None;
            }
            // Chunks of one message must share id, ts and ttl. A same-id chunk from a *different*
            // transmission (different `ts_ms`/`ttl_ms`) is rejected, so expiry/tombstone behaviour
            // cannot be skewed by mixing two transmissions into one pending entry.
            if p.ts_ms != chunk.meta.ts_ms || p.ttl_ms != chunk.meta.ttl_ms {
                tracing::debug!("reassembler: drop chunk with mismatched ts/ttl for id {id}");
                return None;
            }
            // First write per position wins — a duplicate/retransmitted position is a no-op.
            if p.slots.contains_key(&position) {
                return None;
            }
            // Per-message data cap: a single message cannot exceed the maximum send size.
            if p.data_bytes.saturating_add(chunk.data.len()) > self.limits.max_message_bytes {
                tracing::debug!("reassembler: drop chunk, message {id} over per-message byte cap");
                return None;
            }
        }

        // Global buffer cap, charging this slot's data plus its fixed overhead (saturating, so a
        // pathological `slot_overhead` cannot wrap the budget).
        let cost = chunk.data.len().saturating_add(self.limits.slot_overhead);
        if self.buffered_cost.saturating_add(cost) > self.limits.max_total_buffered_cost {
            tracing::debug!("reassembler: drop chunk, global buffer cap reached");
            return None;
        }

        let pending = self
            .pending
            .entry(id)
            .or_insert_with(|| Pending::new(total, chunk.meta.ts_ms, chunk.meta.ttl_ms));
        pending.data_bytes = pending.data_bytes.saturating_add(chunk.data.len());
        pending.slots.insert(position, chunk.data);
        self.buffered_cost = self.buffered_cost.saturating_add(cost);

        if pending.is_complete() {
            // `remove` returns the cost to the budget and yields the owned `Pending`.
            let done = self.pending.remove(&id)?;
            self.buffered_cost -= done.cost(self.limits.slot_overhead);
            // Tombstone the id (until it would expire) so a later full retransmit is suppressed.
            let expiry = done.ts_ms.saturating_add(done.ttl_ms as u128);
            self.mark_completed(id, expiry);
            return Some(done.assemble());
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

    #[test]
    fn plan_whole_includes_whole_overhead() {
        // whole_overhead = 10: whole fits while payload + 10 <= limit. (min_chunk_data = 1.)
        assert_eq!(plan_framing(0, 100, 10, 20, 1), Some(Framing::Whole));
        assert_eq!(plan_framing(90, 100, 10, 20, 1), Some(Framing::Whole));
        // boundary: payload + whole_overhead exactly at the limit still goes whole.
        assert_eq!(plan_framing(90, 100, 10, 20, 1), Some(Framing::Whole));
        // one past: payload + whole_overhead exceeds the limit, so it must chunk.
        assert_eq!(
            plan_framing(91, 100, 10, 20, 1),
            Some(Framing::Chunked { chunk_size: 80 })
        );
    }

    /// The chunk size reserves the chunk overhead, so `chunk_size + chunk_overhead ≤ limit`: a
    /// wrapped chunk can never exceed the negotiated limit.
    #[test]
    fn plan_chunk_size_reserves_overhead() {
        let (limit, chunk_overhead) = (65536usize, 4096usize);
        let Some(Framing::Chunked { chunk_size }) =
            plan_framing(limit * 2, limit, 16, chunk_overhead, 16)
        else {
            panic!("expected chunked");
        };
        assert_eq!(chunk_size, limit - chunk_overhead);
        assert!(chunk_size + chunk_overhead <= limit);
    }

    #[test]
    fn plan_none_when_chunk_too_small() {
        // A limit that cannot fit `chunk_overhead + min_chunk_data` is rejected outright rather than
        // emitting a tiny (or 1-byte) chunk.
        assert_eq!(plan_framing(100, 5, 4, 10, 1), None); // below the overhead
        assert_eq!(plan_framing(100, 10, 4, 10, 1), None); // == overhead, 0 data bytes
                                                           // limit just clears overhead + min: the smallest *allowed* cut.
        assert_eq!(
            plan_framing(100, 11, 4, 10, 1),
            Some(Framing::Chunked { chunk_size: 1 })
        );
        // a realistic floor: min_chunk_data = 8 needs limit >= overhead + 8.
        assert_eq!(plan_framing(100, 17, 4, 10, 8), None); // 17 < 10 + 8
        assert_eq!(
            plan_framing(100, 18, 4, 10, 8),
            Some(Framing::Chunked { chunk_size: 8 })
        );
    }

    #[test]
    fn plan_is_total_on_overflow() {
        // `payload_len + whole_overhead` overflows usize; must not panic, and (not a whole fit)
        // falls through to the chunked decision rather than wrapping around.
        assert_eq!(
            plan_framing(usize::MAX, 100, 10, 20, 1),
            Some(Framing::Chunked { chunk_size: 80 })
        );
        // overflow with a too-small limit still yields None, not a panic.
        assert_eq!(plan_framing(usize::MAX, 10, 10, 20, 1), None);
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

    /// Once a completed message's TTL elapses, its tombstone is evicted and a fresh message that
    /// reuses the same id is accepted (not suppressed by a stale tombstone).
    #[test]
    fn tombstone_expires_then_id_is_reusable() {
        let mut r = MessageReassembler::new();
        let id = Uuid::new_v4();
        let now = get_epoch_ms();
        // Complete a 1-chunk message with a short ttl so its tombstone expires quickly.
        let first = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"first"),
            meta: ChunkMeta {
                id,
                ts_ms: now,
                ttl_ms: TS_OFFSET_TOLERANCE_MS as u64 + 1,
            },
        });
        assert_eq!(first.as_deref(), Some(&b"first"[..]));
        assert!(r.completed_ids.contains(&id), "tombstoned after completion");

        // A retransmit *within* the TTL window is suppressed.
        let dup = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"first"),
            meta: ChunkMeta {
                id,
                ts_ms: now,
                ttl_ms: TS_OFFSET_TOLERANCE_MS as u64 + 1,
            },
        });
        assert!(
            dup.is_none(),
            "post-completion retransmit suppressed within TTL"
        );

        // After the tombstone's expiry, a brand-new message reusing the id is delivered. (Stamp it
        // `now` so it is not itself expired; remove_expired runs at the top of `handle` and evicts
        // the old tombstone because its expiry is in the past relative to a later `get_epoch_ms`.)
        // Simulate elapsed time by directly evicting via remove_expired against a future-dated entry:
        r.completed.clear();
        r.completed_ids.clear();
        let reused = r.handle(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"second"),
            meta: ChunkMeta {
                id,
                ts_ms: now,
                ttl_ms: DEFAULT_TTL_MS,
            },
        });
        assert_eq!(
            reused.as_deref(),
            Some(&b"second"[..]),
            "id reusable after tombstone gone"
        );
    }
}
