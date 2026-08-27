use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::Bytes;
use uuid::Uuid;

use super::Chunk;
use super::ReassemblyLimits;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::fair_admission::try_reserve_atomic;
use crate::utils::get_epoch_ms;

/// One message being reassembled: the chunks seen so far, keyed by position.
pub(super) struct Pending {
    /// total number of chunks the message claims (from `chunk[1]`).
    total: usize,
    /// received positions -> bytes. A `BTreeMap` dedups by position (first write wins) and keeps
    /// the data ordered, so assembly is a single in-order concat.
    pub(super) slots: BTreeMap<usize, Bytes>,
    /// running sum of buffered data bytes, so the per-message cap is O(1) to check.
    pub(super) data_bytes: usize,
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
    /// `0..total`, so `slots.len() == total` iff the present set is exactly `{0..total-1}`.
    fn is_complete(&self) -> bool {
        self.slots.len() == self.total
    }

    /// Buffered cost charged to the global budget: data bytes plus `slot_overhead` per slot.
    /// Saturating arithmetic, so adversarial limit values can never overflow/wrap the budget -
    /// an overflowing cost simply saturates to `usize::MAX` and is rejected as over-budget.
    pub(super) fn cost(&self, slot_overhead: usize) -> usize {
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
    pub(super) pending: HashMap<Uuid, Pending>,
    /// Sum of `Pending::cost(..)` over this peer's `pending` entries.
    pub(super) buffered_cost: usize,
    /// Tombstones for ids that have already been delivered, each paired with its expiry
    /// (`ts_ms + ttl_ms`). A chunk for one of these is dropped, so a post-completion retransmit of a
    /// whole message is not re-assembled and delivered again. `VecDeque` for FIFO/TTL eviction, the
    /// `HashSet` for an O(1) membership check; the two are kept in lockstep.
    completed: VecDeque<(Uuid, u128)>,
    pub(super) completed_ids: HashSet<Uuid>,
    /// The bounds enforced on every incoming chunk.
    limits: ReassemblyLimits,
    budget: Arc<ReassemblyBudget>,
}

/// Node-wide retained chunk cost shared by every per-peer reassembler.
pub(crate) struct ReassemblyBudget {
    pub(super) buffered_cost: AtomicUsize,
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
            completed: VecDeque::new(),
            completed_ids: HashSet::new(),
            // Clamp nonsensical caps so a caller cannot disable an invariant (e.g. a `0` cap).
            limits: limits.normalized(),
            budget,
        }
    }

    /// Record `id` as delivered so a later full retransmit (within the TTL window) is suppressed,
    /// dropping the oldest tombstone if the cap is reached. `expiry` is the message's `ts_ms +
    /// ttl_ms` - after it, a retransmit is rejected by the expiry check anyway, so the tombstone
    /// can go.
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

    /// Drop messages whose TTL has elapsed, returning their cost to the budget, and evict
    /// completed-id tombstones that have likewise expired (a retransmit past its expiry is rejected
    /// anyway).
    pub fn remove_expired(&mut self) {
        self.remove_expired_at(get_epoch_ms());
    }

    /// [`remove_expired`](Self::remove_expired) with the clock injected (tests pass a controlled
    /// `now` to drive the real eviction logic).
    pub(crate) fn remove_expired_at(&mut self, now: u128) {
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
        // Evict every expired tombstone, not just a leading run: completion order need not equal
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
    pub(super) fn handle_at(&mut self, chunk: Chunk, now: u128) -> Option<Bytes> {
        match self.handle_retained_at(chunk, now) {
            ReassemblyOutcome::Complete(bytes) => Some(bytes.into_bytes()),
            ReassemblyOutcome::Incomplete | ReassemblyOutcome::Rejected(_) => None,
        }
    }

    fn handle_retained_at(&mut self, chunk: Chunk, now: u128) -> ReassemblyOutcome {
        // Reclaim expired pending entries and tombstones first, before classify reads them, so
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

    /// The pure admission rule: `(state, chunk, now) -> Ok(cost) | Err(reason)`. Borrows `&self`,
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
        // A real message has at least one chunk and every position in `0..total`.
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
        // below, which must hold for the first chunk too, not only once a pending entry exists, or a
        // caller-supplied `max_chunk_data_len > max_message_bytes` could admit an oversized chunk.
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
        let [position, total] = chunk.chunk;
        let mut pending = self
            .pending
            .remove(&id)
            .unwrap_or_else(|| Pending::new(total, chunk.meta.ts_ms, chunk.meta.ttl_ms));
        pending.data_bytes = pending.data_bytes.saturating_add(chunk.data.len());
        pending.slots.insert(position, chunk.data);
        self.buffered_cost = self.buffered_cost.saturating_add(cost);

        if !pending.is_complete() {
            self.pending.insert(id, pending);
            return ReassemblyOutcome::Incomplete;
        }
        let output_cost = pending.data_bytes;
        if !self.budget.try_reserve(output_cost) {
            let dropped_cost = pending.cost(self.limits.slot_overhead);
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
        let done = pending;
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

/// Why a chunk was not admitted - a value, so [`MessageReassembler::classify`] stays a pure total
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
