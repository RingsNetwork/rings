#![deny(missing_docs)]
//! Message framing / chunking. A message larger than the connection's negotiated
//! `max_message_size` is split into MTU-sized [`Chunk`]s on the sender and reassembled on the
//! receiver.
//!
//! NOTE: this is **whole-message** buffering, not MSRP-style (RFC 4975) streaming. There is no
//! incremental delivery: the receiver yields a payload only once *every* chunk has arrived (or
//! drops it on TTL). The outbound scheduler keeps each message class FIFO, so two chunked messages
//! in the same class do not interleave; a higher-priority class may run between chunks while a
//! delivery is pending. The "split into ordered, id-tagged pieces and reassemble" idea is borrowed
//! from MSRP chunking; MSRP interruption semantics are not implemented.
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
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

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
use crate::utils::try_reserve_atomic;

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

    /// Lower-concurrency limits for constrained deployments.
    ///
    /// The per-message ceiling remains protocol-compatible with production.
    /// Constrained nodes instead admit fewer simultaneous messages and only one
    /// maximum-size reassembly, including its contiguous output copy.
    pub fn constrained() -> Self {
        const CONSTRAINED_MESSAGE_BYTES: usize = TRANSPORT_MAX_SIZE;
        const CONSTRAINED_MAX_CHUNKS: usize = CONSTRAINED_MESSAGE_BYTES / MIN_CHUNK_DATA + 1;
        const CONSTRAINED_SLOT_OVERHEAD: usize = 128;
        const CONSTRAINED_TOTAL_COST: usize =
            crate::fair_admission::retained_wire_bytes(CONSTRAINED_MESSAGE_BYTES)
                + CONSTRAINED_MAX_CHUNKS * CONSTRAINED_SLOT_OVERHEAD;

        Self {
            max_pending_messages: 64,
            max_chunk_data_len: MAX_DATA_CHANNEL_MESSAGE_SIZE,
            max_message_bytes: CONSTRAINED_MESSAGE_BYTES,
            max_chunks_per_message: CONSTRAINED_MAX_CHUNKS,
            max_total_buffered_cost: CONSTRAINED_TOTAL_COST,
            slot_overhead: CONSTRAINED_SLOT_OVERHEAD,
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

    /// Pending cost one peer may retain. One maximum-size legitimate message
    /// still fits, while the node-wide budget keeps capacity available to other
    /// peers instead of allowing a single incomplete-chunk flood to consume it.
    fn max_peer_buffered_cost(self) -> usize {
        self.max_message_bytes
            .saturating_add(
                self.max_chunks_per_message
                    .saturating_mul(self.slot_overhead),
            )
            .min(self.max_total_buffered_cost)
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
    /// Serialize chunk to the Rings wire encoding.
    pub fn to_wire(&self) -> Result<Bytes> {
        rings_codec::serialize(self)
            .map(Bytes::from)
            .map_err(Error::CodecSerialize)
    }

    /// Deserialize chunk from the Rings wire encoding.
    pub fn from_wire(data: &[u8]) -> Result<Self> {
        rings_codec::deserialize(data).map_err(Error::CodecDeserialize)
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
/// chunk is validated and charged to both a per-peer pending-cost limit and a node-wide budget, so
/// reassembly memory cannot grow without limit no matter how the load is shaped. Per-chunk data,
/// per-message data, slot overhead, the id count, and the completed-id tombstone set are all capped,
/// and an already-expired chunk is rejected before it can be delivered or buffered.
pub struct MessageReassembler {
    pending: HashMap<Uuid, Pending>,
    /// Sum of `Pending::cost(..)` over this peer's `pending` entries.
    buffered_cost: usize,
    /// Tombstones for ids that have already been delivered, each paired with its expiry
    /// (`ts_ms + ttl_ms`). A chunk for one of these is dropped, so a post-completion retransmit of a
    /// whole message is not re-assembled and delivered again. `VecDeque` for FIFO/TTL eviction, the
    /// `HashSet` for an O(1) membership check; the two are kept in lockstep.
    completed: std::collections::VecDeque<(Uuid, u128)>,
    completed_ids: std::collections::HashSet<Uuid>,
    /// The bounds enforced on every incoming chunk.
    limits: ReassemblyLimits,
    budget: Arc<ReassemblyBudget>,
}

/// Node-wide retained chunk cost shared by every per-peer reassembler.
pub(crate) struct ReassemblyBudget {
    buffered_cost: AtomicUsize,
    limit: usize,
}

impl ReassemblyBudget {
    pub(crate) fn new(limits: ReassemblyLimits) -> Self {
        Self {
            buffered_cost: AtomicUsize::new(0),
            limit: limits.normalized().max_total_buffered_cost,
        }
    }

    fn try_reserve(&self, cost: usize) -> bool {
        try_reserve_atomic(&self.buffered_cost, cost, self.limit)
    }

    fn release(&self, cost: usize) {
        if self
            .buffered_cost
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(cost)
            })
            .is_err()
        {
            tracing::error!(cost, "reassembly budget release exceeded retained cost");
        }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn buffered_cost_for_test(&self) -> usize {
        self.buffered_cost.load(Ordering::Acquire)
    }
}

/// Completed bytes that remain charged to the node budget until core admission takes ownership.
pub(crate) struct RetainedReassembly {
    bytes: Bytes,
    budget: Arc<ReassemblyBudget>,
    cost: usize,
}

/// Result of applying one chunk to a reassembler.
pub(crate) enum ReassemblyOutcome {
    /// The chunk was admitted but the message is not complete yet.
    Incomplete,
    /// The chunk completed a message whose output remains budget-charged.
    Complete(RetainedReassembly),
    /// The chunk was rejected without mutating retained reassembly state.
    Rejected(ReassemblyRejection),
}

/// Whether a rejected chunk is evidence about the remote peer or local state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReassemblyRejection {
    /// The wire shape, timestamp, metadata, or size is remotely invalid.
    Invalid,
    /// Local per-peer or node-wide reassembly capacity was exhausted.
    Capacity,
    /// The chunk repeats a position or a message already accepted.
    Replay,
    /// A local reassembler invariant failed after admission.
    LocalInvariant,
}

impl RetainedReassembly {
    fn into_bytes(mut self) -> Bytes {
        self.budget.release(self.cost);
        self.cost = 0;
        std::mem::take(&mut self.bytes)
    }
}

impl AsRef<[u8]> for RetainedReassembly {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for RetainedReassembly {
    fn drop(&mut self) {
        self.budget.release(self.cost);
    }
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
        let budget = Arc::new(ReassemblyBudget::new(limits));
        Self::with_limits_and_budget(limits, budget)
    }

    /// Empty per-peer reassembler charged to a node-wide shared budget.
    pub(crate) fn with_limits_and_budget(
        limits: ReassemblyLimits,
        budget: Arc<ReassemblyBudget>,
    ) -> Self {
        Self {
            pending: HashMap::new(),
            buffered_cost: 0,
            completed: std::collections::VecDeque::new(),
            completed_ids: std::collections::HashSet::new(),
            // Clamp nonsensical caps so a caller cannot disable an invariant (e.g. a `0` cap).
            limits: limits.normalized(),
            budget,
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
        let budget = &self.budget;
        let slot_overhead = self.limits.slot_overhead;
        self.pending.retain(|_, p| {
            let alive = p.ts_ms.saturating_add(p.ttl_ms as u128) > now;
            if !alive {
                let cost = p.cost(slot_overhead);
                *buffered_cost = buffered_cost.saturating_sub(cost);
                budget.release(cost);
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
            let cost = p.cost(self.limits.slot_overhead);
            self.buffered_cost = self.buffered_cost.saturating_sub(cost);
            self.budget.release(cost);
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

    /// Accept one chunk while retaining the completed output's node-wide budget charge.
    #[cfg(test)]
    pub(crate) fn handle_retained(&mut self, chunk: Chunk) -> Option<RetainedReassembly> {
        match self.handle_retained_outcome(chunk) {
            ReassemblyOutcome::Complete(bytes) => Some(bytes),
            ReassemblyOutcome::Incomplete | ReassemblyOutcome::Rejected(_) => None,
        }
    }

    /// Accept one chunk while preserving incomplete, complete, and rejected states.
    pub(crate) fn handle_retained_outcome(&mut self, chunk: Chunk) -> ReassemblyOutcome {
        self.handle_retained_at(chunk, get_epoch_ms())
    }

    /// [`handle`](Self::handle) with the clock injected, so tests drive expiry/admission against a
    /// controlled `now` through the real production path instead of poking internal state.
    fn handle_at(&mut self, chunk: Chunk, now: u128) -> Option<Bytes> {
        match self.handle_retained_at(chunk, now) {
            ReassemblyOutcome::Complete(bytes) => Some(bytes.into_bytes()),
            ReassemblyOutcome::Incomplete | ReassemblyOutcome::Rejected(_) => None,
        }
    }

    fn handle_retained_at(&mut self, chunk: Chunk, now: u128) -> ReassemblyOutcome {
        // Reclaim expired pending entries and tombstones FIRST — before classify reads them — so
        // invalid traffic still frees memory and an expired tombstone cannot suppress a fresh
        // message that reuses its id after the TTL window.
        self.remove_expired_at(now);
        match self.classify(&chunk, now) {
            Ok(cost) => self.admit(chunk, cost),
            Err(reason) => {
                tracing::debug!(?reason, id = ?chunk.meta.id, "reassembler dropped chunk");
                ReassemblyOutcome::Rejected(reason.rejection())
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
                if let Some(existing) = p.slots.get(&position) {
                    return if existing == &chunk.data {
                        Err(Rejected::DuplicatePosition)
                    } else {
                        Err(Rejected::ConflictingPosition)
                    };
                }
                p.data_bytes
            }
        };
        // Per-message data cap, enforced uniformly across the first and subsequent chunks.
        if buffered_for_id.saturating_add(chunk.data.len()) > self.limits.max_message_bytes {
            return Err(Rejected::PerMessageBytes);
        }

        // Cost charged to both the peer and node budgets: this slot's data plus its fixed overhead.
        // Saturating arithmetic keeps a pathological `slot_overhead` from wrapping either limit.
        let cost = chunk.data.len().saturating_add(self.limits.slot_overhead);
        if self.buffered_cost.saturating_add(cost) > self.limits.max_peer_buffered_cost() {
            return Err(Rejected::PeerBudget);
        }
        Ok(cost)
    }

    /// The sole buffer mutation: insert a [`classify`]-approved `chunk` (charging `cost`), and if it
    /// completes its message, take it out, refund its budget, tombstone the id, and return the
    /// reassembled payload.
    ///
    /// [`classify`]: Self::classify
    fn admit(&mut self, chunk: Chunk, cost: usize) -> ReassemblyOutcome {
        if !self.budget.try_reserve(cost) {
            tracing::debug!(
                reason = ?Rejected::GlobalBudget,
                id = ?chunk.meta.id,
                "reassembler dropped chunk"
            );
            return ReassemblyOutcome::Rejected(ReassemblyRejection::Capacity);
        }
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
            return ReassemblyOutcome::Incomplete;
        }
        let output_cost = pending.data_bytes;
        if !self.budget.try_reserve(output_cost) {
            let Some(dropped) = self.pending.remove(&id) else {
                tracing::error!(
                    ?id,
                    "completed reassembly disappeared before capacity rejection"
                );
                return ReassemblyOutcome::Rejected(ReassemblyRejection::LocalInvariant);
            };
            let dropped_cost = dropped.cost(self.limits.slot_overhead);
            self.buffered_cost = self.buffered_cost.saturating_sub(dropped_cost);
            self.budget.release(dropped_cost);
            tracing::debug!(
                reason = ?Rejected::GlobalBudget,
                ?id,
                output_cost,
                "reassembler dropped completed message before output allocation"
            );
            return ReassemblyOutcome::Rejected(ReassemblyRejection::Capacity);
        }
        let Some(done) = self.pending.remove(&id) else {
            tracing::error!(
                ?id,
                "completed reassembly disappeared before output construction"
            );
            self.budget.release(output_cost);
            return ReassemblyOutcome::Rejected(ReassemblyRejection::LocalInvariant);
        };
        let done_cost = done.cost(self.limits.slot_overhead);
        let expiry = done.ts_ms.saturating_add(done.ttl_ms as u128);
        self.buffered_cost = self.buffered_cost.saturating_sub(done_cost);
        let bytes = done.assemble();
        self.budget.release(done_cost);
        // Tombstone the id until it would expire, so a later full retransmit is suppressed.
        self.mark_completed(id, expiry);
        ReassemblyOutcome::Complete(RetainedReassembly {
            bytes,
            budget: self.budget.clone(),
            cost: output_cost,
        })
    }
}

impl Drop for MessageReassembler {
    fn drop(&mut self) {
        self.budget.release(self.buffered_cost);
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
    /// This position is buffered with different bytes.
    ConflictingPosition,
    /// Admitting would exceed the message's [`ReassemblyLimits::max_message_bytes`].
    PerMessageBytes,
    /// Admitting would exceed this peer's derived pending-cost allowance.
    PeerBudget,
    /// Admitting would exceed the global [`ReassemblyLimits::max_total_buffered_cost`].
    GlobalBudget,
}

impl Rejected {
    const fn rejection(self) -> ReassemblyRejection {
        match self {
            Self::AlreadyCompleted | Self::DuplicatePosition => ReassemblyRejection::Replay,
            Self::PendingFull | Self::PeerBudget | Self::GlobalBudget => {
                ReassemblyRejection::Capacity
            }
            Self::TtlTooLarge
            | Self::FutureTimestamp
            | Self::Expired
            | Self::Malformed
            | Self::TooManyChunks
            | Self::ChunkTooLarge
            | Self::TotalMismatch
            | Self::MetadataMismatch
            | Self::ConflictingPosition
            | Self::PerMessageBytes => ReassemblyRejection::Invalid,
        }
    }
}

#[cfg(test)]
mod test;
