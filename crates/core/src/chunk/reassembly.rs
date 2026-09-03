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
    /// This logical transmission already produced its one peer-attributable failure.
    failure_charged: bool,
    /// Local capacity rejected at least one chunk, so later incompletion is not peer evidence.
    local_capacity_rejected: bool,
    /// Every retained chunk was bound to an authenticated peer at ingress.
    peer_attributable: bool,
}

impl Pending {
    fn new(total: usize, ts_ms: u128, ttl_ms: u64, peer_attributable: bool) -> Self {
        Self {
            total,
            slots: BTreeMap::new(),
            data_bytes: 0,
            ts_ms,
            ttl_ms,
            failure_charged: false,
            local_capacity_rejected: false,
            peer_attributable,
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

/// Stable identity of one logical transmission. A UUID may be reused after the
/// prior transmission's TTL, so terminal evidence also includes its timestamp
/// and TTL rather than blocking that UUID indefinitely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LogicalTransmission {
    id: Uuid,
    ts_ms: u128,
    ttl_ms: u64,
}

impl LogicalTransmission {
    const fn new(id: Uuid, ts_ms: u128, ttl_ms: u64) -> Self {
        Self { id, ts_ms, ttl_ms }
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
/// per-message data, slot overhead, the pending/terminal id counts, and the completed-id tombstone
/// set are all capped, and an already-expired chunk is rejected before it can be delivered or
/// buffered.
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
    /// Failed or expired logical transmissions retained until a bounded local horizon. Once a
    /// transmission has produced a terminal failure, later chunks with the same UUID, timestamp,
    /// and TTL are replays rather than a second failure or a successful delivery. New failures are
    /// fail-open (not attributed) while this bounded set is full, so hostile UUID rotation cannot
    /// make memory unbounded.
    failed: VecDeque<(LogicalTransmission, u128)>,
    failed_ids: HashSet<LogicalTransmission>,
    /// When invalid-terminal tracking saturates, new invalid arrivals fail open and untracked
    /// already-scored expiries remain replays until this horizon. This prevents a retained expiry
    /// from being charged again merely because an older tombstone drains first.
    failure_tracking_saturated_until: u128,
    /// Transmissions rejected by local capacity before any pending state was retained. Their ids
    /// are blocked until expiry, preventing a later tail from becoming an incomplete message that
    /// is incorrectly attributed to the peer.
    capacity_rejected: VecDeque<(Uuid, u128)>,
    capacity_rejected_ids: HashSet<Uuid>,
    /// When the bounded capacity-id set is full, all new ids are locally rejected until every
    /// untracked rejection that extended this horizon must itself be stale. Existing pending ids
    /// can still make progress, so this cannot hide their genuine expiry failures.
    capacity_tracking_saturated_until: u128,
    /// The bounds enforced on every incoming chunk.
    limits: ReassemblyLimits,
    budget: Arc<ReassemblyBudget>,
}

/// Node-wide retained chunk cost shared by every per-peer reassembler.
pub(crate) struct ReassemblyBudget {
    pub(super) buffered_cost: AtomicUsize,
    limit: usize,
    /// Bumped after every applied reservation or release.
    applied: crate::utils::GenerationWitness,
}

impl ReassemblyBudget {
    pub(crate) fn new(limits: ReassemblyLimits) -> Self {
        Self {
            buffered_cost: AtomicUsize::new(0),
            limit: limits.normalized().max_total_buffered_cost,
            applied: crate::utils::GenerationWitness::default(),
        }
    }

    fn try_reserve(&self, cost: usize) -> bool {
        let reserved = try_reserve_atomic(&self.buffered_cost, cost, self.limit);
        if reserved {
            self.applied.bump();
        }
        reserved
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
        self.applied.bump();
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn buffered_cost_for_test(&self) -> usize {
        self.buffered_cost.load(Ordering::Acquire)
    }

    /// Resolve once `predicate` holds over the retained cost, re-checked on
    /// every reservation or release.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_buffered_cost_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.applied
            .await_until(|_generation| predicate(self.buffered_cost_for_test()))
            .await;
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
    /// The chunk is stale or repeats a position or a message already accepted.
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
            failed: VecDeque::new(),
            failed_ids: HashSet::new(),
            failure_tracking_saturated_until: 0,
            capacity_rejected: VecDeque::new(),
            capacity_rejected_ids: HashSet::new(),
            capacity_tracking_saturated_until: 0,
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
        let _ = self.remove_expired_at(get_epoch_ms());
    }

    /// Return whether any incomplete logical message is still retained.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Drop close-time state that cannot produce a future peer-attributable failure.
    ///
    /// No more chunks can arrive after the owning mailbox closes, so completed
    /// and rejected-id tombstones have no purpose. Incomplete messages are
    /// retained only when their original authenticated ingress can still yield
    /// one failure at TTL; all other buffers release their shared budget now.
    pub(crate) fn prepare_for_close(&mut self) -> bool {
        let buffered_cost = &mut self.buffered_cost;
        let budget = &self.budget;
        let slot_overhead = self.limits.slot_overhead;
        self.pending.retain(|_, pending| {
            let retained = pending.peer_attributable
                && !(pending.failure_charged || pending.local_capacity_rejected);
            if !retained {
                let cost = pending.cost(slot_overhead);
                *buffered_cost = buffered_cost.saturating_sub(cost);
                budget.release(cost);
            }
            retained
        });
        self.clear_terminal_history();
        !self.pending.is_empty()
    }

    /// Release every close-time pending buffer when no timer can deliver TTL cleanup.
    pub(crate) fn discard_after_close_timer_failure(&mut self) {
        for pending in self.pending.values() {
            self.budget.release(pending.cost(self.limits.slot_overhead));
        }
        self.pending.clear();
        self.buffered_cost = 0;
        self.clear_terminal_history();
    }

    fn clear_terminal_history(&mut self) {
        self.completed.clear();
        self.completed_ids.clear();
        self.failed.clear();
        self.failed_ids.clear();
        self.failure_tracking_saturated_until = 0;
        self.capacity_rejected.clear();
        self.capacity_rejected_ids.clear();
        self.capacity_tracking_saturated_until = 0;
    }

    /// [`remove_expired`](Self::remove_expired) with the clock injected (tests pass a controlled
    /// `now` to drive the real eviction logic).
    pub(crate) fn remove_expired_at(&mut self, now: u128) -> usize {
        self.evict_expired_terminal_history(now);
        let mut expired_count = 0_usize;
        let mut expired_transmissions = Vec::new();
        let buffered_cost = &mut self.buffered_cost;
        let budget = &self.budget;
        let slot_overhead = self.limits.slot_overhead;
        self.pending.retain(|id, p| {
            let alive = p.ts_ms.saturating_add(p.ttl_ms as u128) > now;
            if !alive {
                let cost = p.cost(slot_overhead);
                *buffered_cost = buffered_cost.saturating_sub(cost);
                budget.release(cost);
                expired_transmissions.push(LogicalTransmission::new(*id, p.ts_ms, p.ttl_ms));
                if !(p.failure_charged || p.local_capacity_rejected) && p.peer_attributable {
                    expired_count = expired_count.saturating_add(1);
                }
            }
            alive
        });
        // Keep a bounded terminal witness after removing the pending buffer. This distinguishes a
        // late chunk for an already-scored expiry from a first expired-on-arrival chunk, which is
        // invalid evidence. At capacity `mark_failed_transmission` fails open for reputation by
        // classifying the event as Replay without growing memory. Its saturation horizon keeps
        // that classification stable if an older tombstone drains first; after the bounded
        // horizon, exact-once history is intentionally forgotten with the tombstones.
        for transmission in expired_transmissions {
            if !self.mark_failed_transmission(transmission, now) {
                // This expiry has already contributed to `expired_count`. Keep its untracked
                // witness horizon alive even when a prior saturation interval was already active.
                self.extend_failure_tracking_saturation(now);
            }
        }
        expired_count
    }

    fn evict_expired_terminal_history(&mut self, now: u128) {
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
        let failed_ids = &mut self.failed_ids;
        self.failed.retain(|&(transmission, expiry)| {
            let alive = expiry > now;
            if !alive {
                failed_ids.remove(&transmission);
            }
            alive
        });
        let capacity_rejected_ids = &mut self.capacity_rejected_ids;
        self.capacity_rejected.retain(|&(id, expiry)| {
            let alive = expiry > now;
            if !alive {
                capacity_rejected_ids.remove(&id);
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
    /// admission verdict, and apply it. Accepted data mutation stays in `admit`; the shell records
    /// only bounded terminal/capacity evidence for rejected chunks so local loss cannot become a
    /// peer failure and one logical id cannot produce conflicting terminal outcomes.
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
    #[cfg(test)]
    pub(crate) fn handle_retained_outcome(&mut self, chunk: Chunk) -> ReassemblyOutcome {
        self.handle_retained_at(chunk, get_epoch_ms()).0
    }

    #[cfg(test)]
    pub(crate) fn handle_retained_outcome_at(
        &mut self,
        chunk: Chunk,
        now: u128,
    ) -> ReassemblyOutcome {
        self.handle_retained_at(chunk, now).0
    }

    /// [`handle`](Self::handle) with the clock injected, so tests drive expiry/admission against a
    /// controlled `now` through the real production path instead of poking internal state.
    pub(super) fn handle_at(&mut self, chunk: Chunk, now: u128) -> Option<Bytes> {
        match self.handle_retained_at(chunk, now).0 {
            ReassemblyOutcome::Complete(bytes) => Some(bytes.into_bytes()),
            ReassemblyOutcome::Incomplete | ReassemblyOutcome::Rejected(_) => None,
        }
    }

    fn handle_retained_at(&mut self, chunk: Chunk, now: u128) -> (ReassemblyOutcome, usize) {
        self.handle_retained_at_with_attribution(chunk, now, true)
    }

    /// Accept one chunk against the caller's clock and also report how many older incomplete
    /// logical messages expired before admission; the runtime adapter records one peer failure
    /// per expired message.
    ///
    /// The inbound processor supplies its injected reassembly clock so admission freshness and
    /// cleanup expiry agree on one `now`.
    pub(crate) fn handle_retained_at_with_attribution(
        &mut self,
        chunk: Chunk,
        now: u128,
        peer_attributable: bool,
    ) -> (ReassemblyOutcome, usize) {
        // Reclaim expired pending entries and tombstones first, before classify reads them, so
        // invalid traffic still frees memory and an expired tombstone cannot suppress a fresh
        // message that reuses its id after the TTL window.
        let expired = self.remove_expired_at(now);
        let outcome = match self.classify(&chunk, now) {
            Ok(cost) => self.admit(chunk, cost, peer_attributable),
            Err(reason) => {
                tracing::debug!(?reason, id = ?chunk.meta.id, "reassembler dropped chunk");
                let rejection = match reason.rejection() {
                    ReassemblyRejection::Invalid => {
                        if self.mark_logical_failure(&chunk, now) {
                            ReassemblyRejection::Invalid
                        } else {
                            ReassemblyRejection::Replay
                        }
                    }
                    ReassemblyRejection::Capacity => {
                        self.mark_pending_capacity_rejection(&chunk);
                        ReassemblyRejection::Capacity
                    }
                    ReassemblyRejection::Replay => ReassemblyRejection::Replay,
                };
                ReassemblyOutcome::Rejected(rejection)
            }
        };
        (outcome, expired)
    }

    /// The pure admission rule: `(state, chunk, now) -> Ok(cost) | Err(reason)`. Borrows `&self`,
    /// mutates nothing, does no I/O. On success it returns the buffered cost [`admit`] must charge;
    /// on failure a typed [`Rejected`] reason. Validating the existing pending entry here, before
    /// accepted-data mutation, is what keeps buffer accounting exact. The surrounding shell may
    /// retain bounded failure/capacity evidence after this pure verdict.
    ///
    /// [`admit`]: Self::admit
    fn classify(&self, chunk: &Chunk, now: u128) -> std::result::Result<usize, Rejected> {
        let meta = &chunk.meta;
        let transmission = LogicalTransmission::new(meta.id, meta.ts_ms, meta.ttl_ms);
        if self
            .pending
            .get(&meta.id)
            .is_some_and(|pending| pending.failure_charged)
            || self.failed_ids.contains(&transmission)
        {
            return Err(Rejected::AlreadyFailed);
        }
        if self.capacity_rejected_ids.contains(&meta.id) {
            return Err(Rejected::CapacityRejectedId);
        }
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
            None if self.capacity_tracking_saturated_until > now => {
                return Err(Rejected::CapacityTrackingFull);
            }
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

    fn mark_logical_failure(&mut self, chunk: &Chunk, now: u128) -> bool {
        let meta = &chunk.meta;
        if let Some(pending) = self.pending.get_mut(&meta.id) {
            if pending.failure_charged {
                return false;
            }
            pending.failure_charged = true;
            return true;
        }

        self.mark_failed_transmission(
            LogicalTransmission::new(meta.id, meta.ts_ms, meta.ttl_ms),
            now,
        )
    }

    fn mark_failed_transmission(&mut self, transmission: LogicalTransmission, now: u128) -> bool {
        if self.failed_ids.contains(&transmission) || self.failure_tracking_saturated_until > now {
            return false;
        }
        if self.failed_ids.len() >= self.limits.max_completed_ids {
            self.extend_failure_tracking_saturation(now);
            return false;
        }
        // Invalid timestamps/TTLs must not pin terminal state longer than the protocol maximum.
        // Using a local horizon also gives a well-defined reuse point for a malformed identity.
        let expiry = now.saturating_add(MAX_TTL_MS as u128);
        self.failed_ids.insert(transmission);
        self.failed.push_back((transmission, expiry));
        true
    }

    fn extend_failure_tracking_saturation(&mut self, now: u128) {
        self.failure_tracking_saturated_until = self
            .failure_tracking_saturated_until
            .max(now.saturating_add(MAX_TTL_MS as u128));
    }

    fn mark_pending_capacity_rejection(&mut self, chunk: &Chunk) {
        let meta = &chunk.meta;
        if let Some(pending) = self.pending.get_mut(&meta.id) {
            if pending.ts_ms == meta.ts_ms && pending.ttl_ms == meta.ttl_ms {
                pending.local_capacity_rejected = true;
                return;
            }
        }
        self.mark_capacity_rejected_id(meta.id, meta.ts_ms, meta.ttl_ms);
    }

    fn mark_capacity_rejected_id(&mut self, id: Uuid, ts_ms: u128, ttl_ms: u64) {
        let expiry = ts_ms.saturating_add(ttl_ms.min(MAX_TTL_MS) as u128);
        if self.capacity_rejected_ids.contains(&id) {
            return;
        }
        if self.capacity_rejected_ids.len() >= self.limits.max_completed_ids {
            self.capacity_tracking_saturated_until =
                self.capacity_tracking_saturated_until.max(expiry);
            return;
        }
        self.capacity_rejected_ids.insert(id);
        self.capacity_rejected.push_back((id, expiry));
    }

    /// The sole buffer mutation: insert a [`classify`]-approved `chunk` (charging `cost`), and if it
    /// completes its message, take it out, refund its budget, tombstone the id, and return the
    /// reassembled payload.
    ///
    /// [`classify`]: Self::classify
    fn admit(&mut self, chunk: Chunk, cost: usize, peer_attributable: bool) -> ReassemblyOutcome {
        if !self.budget.try_reserve(cost) {
            self.mark_pending_capacity_rejection(&chunk);
            tracing::debug!(
                reason = ?Rejected::GlobalBudget,
                id = ?chunk.meta.id,
                "reassembler dropped chunk"
            );
            return ReassemblyOutcome::Rejected(ReassemblyRejection::Capacity);
        }
        let id = chunk.meta.id;
        let [position, total] = chunk.chunk;
        let mut pending = self.pending.remove(&id).unwrap_or_else(|| {
            Pending::new(
                total,
                chunk.meta.ts_ms,
                chunk.meta.ttl_ms,
                peer_attributable,
            )
        });
        pending.peer_attributable &= peer_attributable;
        pending.data_bytes = pending.data_bytes.saturating_add(chunk.data.len());
        pending.slots.insert(position, chunk.data);
        self.buffered_cost = self.buffered_cost.saturating_add(cost);

        if !pending.is_complete() {
            self.pending.insert(id, pending);
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            crate::simulation::observe_reassembly_capacity(
                self.budget.buffered_cost.load(Ordering::Acquire),
                self.budget.limit,
                self.buffered_cost,
                self.limits.max_peer_buffered_cost(),
                self.pending.len(),
                self.limits.max_pending_messages,
            );
            return ReassemblyOutcome::Incomplete;
        }
        let output_cost = pending.data_bytes;
        if !self.budget.try_reserve(output_cost) {
            self.mark_capacity_rejected_id(id, pending.ts_ms, pending.ttl_ms);
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
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::observe_reassembly_capacity(
            self.budget.buffered_cost.load(Ordering::Acquire),
            self.budget.limit,
            self.buffered_cost,
            self.limits.max_peer_buffered_cost(),
            self.pending.len().saturating_add(1),
            self.limits.max_pending_messages,
        );
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
    /// This in-flight logical transmission already produced its failure outcome.
    AlreadyFailed,
    /// This logical id was previously rejected by local capacity before retaining state.
    CapacityRejectedId,
    /// Bounded local capacity-history tracking is saturated, so new ids fail closed locally.
    CapacityTrackingFull,
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
            Self::AlreadyCompleted
            | Self::AlreadyFailed
            | Self::DuplicatePosition => ReassemblyRejection::Replay,
            Self::CapacityRejectedId
            | Self::CapacityTrackingFull
            | Self::PendingFull
            | Self::PeerBudget
            | Self::GlobalBudget => ReassemblyRejection::Capacity,
            Self::TtlTooLarge
            | Self::FutureTimestamp
            // An expired-on-arrival id has no retained expiry outcome. Treat it
            // as invalid once; `mark_logical_failure` tombstones duplicate ids.
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
