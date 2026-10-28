use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use bytes::Bytes;
use rings_transport::core::pool::ChannelLane;
use uuid::Uuid;

use super::progress::Progress;
use super::progress::RateFloor;
use super::Chunk;
use super::ReassemblyLimits;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::utils::get_epoch_ms;

/// One message being reassembled: the chunks seen so far, keyed by position, and the charge
/// its first chunk took for the whole message.
pub(super) struct Pending<C> {
    /// total number of chunks the message claims (from `chunk[1]`).
    total: usize,
    /// `d₀`, the data length of the first chunk, which no later chunk may exceed, so the
    /// message's data never passes the `n·d₀` its first chunk was charged for.
    first_len: usize,
    /// received positions -> bytes. A `BTreeMap` dedups by position (first write wins) and keeps
    /// the data ordered, so assembly is a single in-order concat.
    pub(super) slots: BTreeMap<usize, Bytes>,
    /// creation time / ttl of the first chunk seen, used for TTL eviction.
    ts_ms: u128,
    ttl_ms: u64,
    /// This logical transmission already produced its one peer-attributable failure.
    failure_charged: bool,
    /// Every retained chunk was bound to an authenticated peer at ingress.
    peer_attributable: bool,
    /// The lane the first chunk arrived on, which every later chunk must share.
    carrier: Option<ChannelLane>,
    /// The charge the first chunk took for the whole message (load-bounds paper, Law 4): held
    /// while the message is reassembled and handed on with it, released by drop.
    charge: C,
    /// The message's credit-time ledger, which its rate floor reads.
    progress: Progress,
}

impl<C> Pending<C> {
    /// The rate floor of this message under `limits`.
    fn rate_floor(&self, limits: &ReassemblyLimits) -> RateFloor {
        RateFloor {
            rate: limits.rate_floor,
            chunk: u64::try_from(self.first_len).unwrap_or(u64::MAX),
            grace_chunks: limits.grace_chunks,
        }
    }

    /// Whether the message is past its own expiry at `now`.
    fn ttl_expired(&self, now: u128) -> bool {
        self.ts_ms.saturating_add(u128::from(self.ttl_ms)) <= now
    }

    /// Complete iff every position has arrived. Each inserted position is unique (map key) and in
    /// `0..total`, so `slots.len() == total` iff the present set is exactly `{0..total-1}`.
    fn is_complete(&self) -> bool {
        self.slots.len() == self.total
    }

    /// Whether `chunk`, carried on `carrier`, joins this in-flight message: it agrees on the
    /// message's shape and provenance, is no longer than the first chunk, and fills a position
    /// still free.
    fn admits(&self, chunk: &Chunk, carrier: Option<ChannelLane>) -> Result<(), Rejected> {
        let [position, total] = chunk.chunk;
        if self.total != total {
            return Err(Rejected::TotalMismatch);
        }
        // Chunks of one message share id+ts+ttl; a same-id chunk from a different transmission
        // must not be merged in (it would skew expiry/tombstone behaviour).
        if self.ts_ms != chunk.meta.ts_ms || self.ttl_ms != chunk.meta.ttl_ms {
            return Err(Rejected::MetadataMismatch);
        }
        // A message's chunks travel one lane; one that changes lanes would take credit on a lane
        // its content does not belong to.
        if self.carrier != carrier {
            return Err(Rejected::CarrierMismatch);
        }
        // The first chunk was charged for `n·d₀`; a longer chunk would pass that charge.
        if chunk.data.len() > self.first_len {
            return Err(Rejected::LongerThanFirst);
        }
        // First write per position wins; a duplicate position is a no-op, not an error.
        match self.slots.get(&position) {
            Some(existing) if existing == &chunk.data => Err(Rejected::DuplicatePosition),
            Some(_) => Err(Rejected::ConflictingPosition),
            None => Ok(()),
        }
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
/// Correct under duplicates / retransmits (first write per position wins *during* assembly),
/// partial delivery (TTL eviction), and a message **fully retransmitted after it already
/// completed**: a completed id is kept as a tombstone until it would expire, so a late re-send
/// within the TTL window is dropped rather than re-assembled and delivered twice.
///
/// **One charge per message** (load-bounds paper, Law 4, #932). A message is opened only by its
/// first chunk, `[0, n]` with data length `d₀`, and only when `(n − 1)·d₀` stays below
/// [`ReassemblyLimits::max_message_bytes`]; the charge `C` that chunk brings (the receiver's
/// mailbox reservation of `n·d₀`) is held with the message. Every later chunk is at most `d₀`
/// long, so the message never outgrows that charge, and the charge a later chunk brings is
/// dropped. The charge leaves with the completed message (`RetainedReassembly`), or is dropped
/// when the message expires or the reassembler closes. The reassembler itself holds no budget
/// and refuses nothing for capacity: its remaining caps (ids, slots, chunk length, message
/// length) are validity bounds an honest sender never meets.
///
/// ```text
/// chunk [0, n], d₀ ── new id, (n − 1)·d₀ < max ──▶ open(charge)        (Incomplete)
/// chunk [i, n], dᵢ ── open, dᵢ ≤ d₀, slot free ───▶ insert; drop charge (Incomplete | Complete)
/// chunk [i > 0, n] ── no open message ────────────▶ Rejected(Invalid | Replay)
/// expiry, stall, close ───────────────────────────▶ drop(message, charge)
/// ```
///
/// **Rate floor.** An open message also expires, attributably, once the bytes it received fall
/// below the rate floor over the credit-time its sender held unused credit for, less one lane
/// window of grace (`progress`; load-bounds paper, Def. 7 and Law 4). The caller reads that
/// credit, per carrier, from its transport; a caller that reads none (`|_| 0`, as the plain API
/// does) never advances the clock, and a message then expires at its TTL alone.
pub struct MessageReassembler<C = ()> {
    pub(super) pending: HashMap<Uuid, Pending<C>>,
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
    /// The bounds enforced on every incoming chunk.
    limits: ReassemblyLimits,
}

/// A completed message and the charge its first chunk took for it.
pub(crate) struct RetainedReassembly<C> {
    bytes: Bytes,
    /// The charge, handed on with the message.
    charge: C,
    /// The lane every chunk of the message arrived on.
    carrier: Option<ChannelLane>,
}

/// Result of applying one chunk to a reassembler.
pub(crate) enum ReassemblyOutcome<C> {
    /// The chunk was admitted but the message is not complete yet.
    Incomplete,
    /// The chunk completed a message, which carries its charge.
    Complete(RetainedReassembly<C>),
    /// The chunk was rejected without mutating retained reassembly state.
    Rejected(ReassemblyRejection),
}

/// Whether a rejected chunk is evidence about the remote peer: no rejection is for capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReassemblyRejection {
    /// The wire shape, timestamp, metadata, or size is remotely invalid.
    Invalid,
    /// The chunk is stale or repeats a position or a message already accepted.
    Replay,
}

impl<C> RetainedReassembly<C> {
    /// The lane every chunk of the message arrived on, for the receiver to check against the
    /// lane its content belongs to.
    pub(crate) const fn carrier(&self) -> Option<ChannelLane> {
        self.carrier
    }

    /// The message's bytes and its charge.
    pub(crate) fn into_parts(self) -> (Bytes, C) {
        (self.bytes, self.charge)
    }
}

impl<C> AsRef<[u8]> for RetainedReassembly<C> {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Default for MessageReassembler {
    fn default() -> Self {
        Self::with_limits(ReassemblyLimits::production())
    }
}

impl MessageReassembler {
    /// Empty reassembler with [`ReassemblyLimits::production`] bounds, holding no charge.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept one chunk. Returns the fully reassembled payload when this chunk completes its
    /// message (which is then forgotten), otherwise `None`.
    pub fn handle(&mut self, chunk: Chunk) -> Option<Bytes> {
        self.handle_at(chunk, get_epoch_ms())
    }

    /// [`handle`](Self::handle) with the clock injected, so tests drive expiry/admission against a
    /// controlled `now` through the real production path instead of poking internal state.
    pub(super) fn handle_at(&mut self, chunk: Chunk, now: u128) -> Option<Bytes> {
        match self
            .handle_charged_at(chunk, (), (now, no_credit), true, None)
            .0
        {
            ReassemblyOutcome::Complete(retained) => Some(retained.into_parts().0),
            ReassemblyOutcome::Incomplete | ReassemblyOutcome::Rejected(_) => None,
        }
    }

    /// Accept one chunk while preserving incomplete, complete, and rejected states.
    #[cfg(test)]
    pub(crate) fn handle_retained_outcome_at(
        &mut self,
        chunk: Chunk,
        now: u128,
    ) -> ReassemblyOutcome<()> {
        self.handle_charged_at(chunk, (), (now, no_credit), true, None)
            .0
    }
}

impl<C> MessageReassembler<C> {
    /// Empty reassembler enforcing the given `limits`, whose messages hold charges `C`. Tests
    /// use this with small limits to exercise the admission rule without giant synthetic
    /// payloads.
    pub fn with_limits(limits: ReassemblyLimits) -> Self {
        Self {
            pending: HashMap::new(),
            completed: VecDeque::new(),
            completed_ids: HashSet::new(),
            failed: VecDeque::new(),
            failed_ids: HashSet::new(),
            failure_tracking_saturated_until: 0,
            // Clamp nonsensical caps so a caller cannot disable an invariant (e.g. a `0` cap).
            limits: limits.normalized(),
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

    /// Drop messages whose TTL has elapsed, with their charges, and evict completed-id
    /// tombstones that have likewise expired (a retransmit past its expiry is rejected anyway).
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
    /// one failure at TTL; all other messages are dropped with their charges now.
    pub(crate) fn prepare_for_close(&mut self) -> bool {
        self.pending
            .retain(|_, pending| pending.peer_attributable && !pending.failure_charged);
        self.clear_terminal_history();
        !self.pending.is_empty()
    }

    /// Drop every close-time message, with its charge, when no timer can deliver TTL cleanup.
    pub(crate) fn discard_after_close_timer_failure(&mut self) {
        self.pending.clear();
        self.clear_terminal_history();
    }

    fn clear_terminal_history(&mut self) {
        self.completed.clear();
        self.completed_ids.clear();
        self.failed.clear();
        self.failed_ids.clear();
        self.failure_tracking_saturated_until = 0;
    }

    /// [`remove_expired`](Self::remove_expired) with the clock injected (tests pass a controlled
    /// `now` to drive the real eviction logic); returns how many expired messages are peer
    /// evidence.
    pub(crate) fn remove_expired_at(&mut self, now: u128) -> usize {
        self.sweep_at(now, no_credit)
    }

    /// Sample every open message at `now`, its sender holding `credit(carrier)` unused, and drop
    /// those past their TTL or below their rate floor, with their charges; returns how many of
    /// them are peer evidence.
    pub(crate) fn sweep_at(
        &mut self,
        now: u128,
        credit: impl Fn(Option<ChannelLane>) -> u64,
    ) -> usize {
        self.evict_expired_terminal_history(now);
        let mut expired_count = 0_usize;
        let mut expired_transmissions = Vec::new();
        let limits = self.limits;
        self.pending.retain(|id, p| {
            let floor = p.rate_floor(&limits);
            p.progress.observe(now, credit(p.carrier), floor);
            let alive = !p.ttl_expired(now) && !p.progress.stalled(floor);
            if !alive {
                expired_transmissions.push(LogicalTransmission::new(*id, p.ts_ms, p.ttl_ms));
                if !p.failure_charged && p.peer_attributable {
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
    }

    /// Forget a message (e.g. after it has been delivered), dropping its charge.
    pub fn remove(&mut self, id: Uuid) {
        self.pending.remove(&id);
    }

    /// Accept one chunk that brought `charge`, against the caller's clock, and also report how
    /// many older incomplete logical messages expired before admission; the runtime adapter
    /// records one peer failure per expired message.
    ///
    /// Imperative shell over a functional core: expire stale state, ask the pure `classify` for an
    /// admission verdict, and apply it. Accepted data mutation stays in `admit`, which keeps
    /// `charge` iff the chunk opens its message; the shell records only bounded terminal evidence
    /// for rejected chunks, so one logical id cannot produce conflicting terminal outcomes.
    pub(crate) fn handle_charged_at(
        &mut self,
        chunk: Chunk,
        charge: C,
        (now, credit): (u128, impl Fn(Option<ChannelLane>) -> u64),
        peer_attributable: bool,
        carrier: Option<ChannelLane>,
    ) -> (ReassemblyOutcome<C>, usize) {
        // Reclaim expired and stalled pending entries and tombstones first, before classify
        // reads them, so invalid traffic still frees memory and an expired tombstone cannot
        // suppress a fresh message that reuses its id after the TTL window.
        let expired = self.sweep_at(now, &credit);
        let outcome = match self.classify(&chunk, now, carrier) {
            Ok(()) => self.admit(
                chunk,
                charge,
                (now, credit(carrier)),
                (peer_attributable, carrier),
            ),
            Err(reason) => {
                tracing::debug!(?reason, id = ?chunk.meta.id, "reassembler dropped chunk");
                let rejection = match reason.rejection() {
                    ReassemblyRejection::Invalid if self.mark_logical_failure(&chunk, now) => {
                        ReassemblyRejection::Invalid
                    }
                    ReassemblyRejection::Invalid | ReassemblyRejection::Replay => {
                        ReassemblyRejection::Replay
                    }
                };
                ReassemblyOutcome::Rejected(rejection)
            }
        };
        (outcome, expired)
    }

    /// The pure admission rule: `(state, chunk, now) -> Ok(()) | Err(reason)`. Borrows `&self`,
    /// mutates nothing, does no I/O; on failure a typed [`Rejected`] reason. The surrounding shell
    /// may retain bounded failure evidence after this pure verdict.
    fn classify(
        &self,
        chunk: &Chunk,
        now: u128,
        carrier: Option<ChannelLane>,
    ) -> Result<(), Rejected> {
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
        match self.pending.get(&meta.id) {
            Some(pending) => pending.admits(chunk, carrier),
            // Only the first chunk, which the receiver charged for the whole message, opens it;
            // an honest sender sends a message's chunks in order on one lane.
            None if position != 0 => Err(Rejected::MissingFirstChunk),
            // An honest connection has at most one message open per lane.
            None if self.pending.len() >= self.limits.max_pending_messages => {
                Err(Rejected::PendingFull)
            }
            // `(n − 1)·d₀ < max`: an honest message of at most `max` bytes cut into chunks of
            // `d₀` has `n = ⌈len / d₀⌉`, so its first `n − 1` chunks hold fewer than `max` bytes.
            None if (total - 1).saturating_mul(chunk.data.len())
                >= self.limits.max_message_bytes =>
            {
                Err(Rejected::PerMessageBytes)
            }
            None => Ok(()),
        }
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

    /// The sole buffer mutation: insert a [`classify`]-approved `chunk`; a first chunk opens its
    /// message with `charge`, any other drops it. If the chunk completes its message, take it
    /// out, tombstone the id, and return the reassembled payload with its charge.
    ///
    /// [`classify`]: Self::classify
    fn admit(
        &mut self,
        chunk: Chunk,
        charge: C,
        (now, credit): (u128, u64),
        (peer_attributable, carrier): (bool, Option<ChannelLane>),
    ) -> ReassemblyOutcome<C> {
        let id = chunk.meta.id;
        let [position, total] = chunk.chunk;
        let received = u64::try_from(chunk.data.len()).unwrap_or(u64::MAX);
        let mut pending = match self.pending.remove(&id) {
            Some(mut pending) => {
                let floor = pending.rate_floor(&self.limits);
                pending.progress.observe(now, credit, floor);
                pending.progress.receive(received);
                pending
            }
            None => Pending {
                total,
                first_len: chunk.data.len(),
                slots: BTreeMap::new(),
                ts_ms: chunk.meta.ts_ms,
                ttl_ms: chunk.meta.ttl_ms,
                failure_charged: false,
                peer_attributable,
                carrier,
                charge,
                progress: Progress::open(now, credit, received),
            },
        };
        pending.peer_attributable &= peer_attributable;
        pending.slots.insert(position, chunk.data);

        if !pending.is_complete() {
            self.pending.insert(id, pending);
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            crate::simulation::observe_reassembly_pending(
                self.pending.len(),
                self.limits.max_pending_messages,
            );
            return ReassemblyOutcome::Incomplete;
        }
        let expiry = pending.ts_ms.saturating_add(pending.ttl_ms as u128);
        // Tombstone the id until it would expire, so a later full retransmit is suppressed.
        self.mark_completed(id, expiry);
        ReassemblyOutcome::Complete(RetainedReassembly {
            bytes: pending.slots.into_values().flatten().collect(),
            charge: pending.charge,
            carrier: pending.carrier,
        })
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
    /// A later chunk of a message no first chunk opened.
    MissingFirstChunk,
    /// A new id, but [`ReassemblyLimits::max_pending_messages`] is already reached.
    PendingFull,
    /// `total` disagrees with the in-flight message's.
    TotalMismatch,
    /// `ts_ms`/`ttl_ms` disagree with the in-flight message's (a different transmission).
    MetadataMismatch,
    /// The chunk arrived on another lane than the in-flight message's first chunk.
    CarrierMismatch,
    /// The chunk is longer than its message's first chunk.
    LongerThanFirst,
    /// This position is already buffered (a duplicate/retransmit).
    DuplicatePosition,
    /// This position is buffered with different bytes.
    ConflictingPosition,
    /// The first chunk claims a message of [`ReassemblyLimits::max_message_bytes`] or more.
    PerMessageBytes,
}

impl Rejected {
    const fn rejection(self) -> ReassemblyRejection {
        match self {
            Self::AlreadyCompleted | Self::AlreadyFailed | Self::DuplicatePosition => {
                ReassemblyRejection::Replay
            }
            Self::TtlTooLarge
            | Self::FutureTimestamp
            // An expired-on-arrival id has no retained expiry outcome. Treat it
            // as invalid once; `mark_logical_failure` tombstones duplicate ids.
            | Self::Expired
            | Self::Malformed
            | Self::TooManyChunks
            | Self::ChunkTooLarge
            | Self::MissingFirstChunk
            | Self::PendingFull
            | Self::TotalMismatch
            | Self::MetadataMismatch
            | Self::CarrierMismatch
            | Self::LongerThanFirst
            | Self::ConflictingPosition
            | Self::PerMessageBytes => ReassemblyRejection::Invalid,
        }
    }
}

/// The credit reading of a caller that reads none: no sender ever holds unused credit, so the
/// rate floor's clock never advances.
const fn no_credit(_carrier: Option<ChannelLane>) -> u64 {
    0
}
