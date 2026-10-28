//! Per-lane credit flow control: the receiver bounds how many frames of each lane it holds, and
//! the sender never exceeds that bound, so receive-side admission never refuses an honest frame.
//!
//! A connection carries [`DATA_CHANNEL_POOL_SIZE`] lanes, each pinned to one ordered data
//! channel (see [`crate::core::pool`]). For every lane, independently, the two ends keep:
//!
//! ```text
//!   sender   S = (committed, reserved, limit, g)      limit    : the greatest credit of g
//!   receiver R = (received, released, advertised,     advertised: the greatest credit granted
//!                 lent, g, shrinking)                 lent      : the share lent by the pool
//!
//!   reserve   : S → S + 1 reservation     if committed + reserved < limit
//!   commit    : one reservation becomes one sent frame (the send became irrevocable)
//!   cancel    : one reservation is returned (the send was abandoned before it was irrevocable)
//!   grant(l,g): limit ← max(limit, l)     if g is the sender's generation   (Credit(l, g))
//!   admit     : received ← received + 1   if received < advertised, otherwise a violation
//!   release   : released ← released + 1                    (the frame left the transport)
//!   advertise : advertised ← released + floor + lent  once it exceeds advertised by a batch,
//!               and never while a shrink is pending
//!   lend(x)   : lent ← lent + x           at most W − floor − lent      (the node's pool)
//!   shrink(l) : g ← g + 1, lent ← l < lent,
//!               send Shrink(min(released + floor + l, advertised), g)   (idle: l = 0)
//!   shrink(L′,g) at the sender: B = max(committed + reserved, L′), limit ← B,
//!               reply Returned(B, g)
//!   returned(B,g) at the receiver: advertised ← B, the shrink ends
//! ```
//!
//! A lane's liability, `advertised − released`, is what the receiver may still have to hold:
//! frames held, in flight, or credited and unused. Its `floor` is always granted; beyond it the
//! node lends from one pool (`callback::link_credit`), so a node's whole liability is bounded
//! whatever the number of connections (load-bounds paper, Law 3). The priority lane's floor is
//! its whole window, so it never borrows (Prop. 3(5)).
//!
//! Credits are cumulative frame indices within a generation, so `grant` is the join of the
//! max-semilattice `(ℕ, max)` on credits of the current generation: idempotent, commutative and
//! associative. A duplicated, stale or reordered credit frame therefore changes nothing a newer
//! one has not already changed, a credit of an older generation is ignored, and the link may
//! treat its credit, shrink and return frames as datagrams.
//!
//! Laws, for an honest sender and every interleaving of sends, deliveries and releases:
//!
//! - **Bound.** `floor ≤ advertised − released ≤ W` while no shrink is pending:
//!   `received ≤ advertised` by admission, every advertised value is
//!   `released + floor + lent ≤ released + W` when made, and a shrink is answered by a bound no
//!   greater than the advertised credit.
//! - **No honest violation.** Frames of a lane arrive in order and at most once, so the
//!   `n`-th arrival was the `n`-th commit; `committed ≤ limit ≤ advertised`, since every limit
//!   the sender holds was advertised in its generation, and a returned bound covers every
//!   frame committed or reserved. Hence `received < advertised` at every honest arrival.
//! - **Progress.** If the receiver releases every frame it admitted, every advertisement is
//!   eventually made, and every credit, shrink and return frame is eventually delivered once,
//!   the sender is never blocked forever: when it has committed all it may and the receiver has
//!   released them all, the next advertisement is `advertised + floor`, at least one frame.
//! - **Isolation.** The state of a connection is the product of its lanes' states, and no
//!   transition of one lane reads another, so one lane's backlog never blocks another lane.
//!
//! These laws are model-checked in `test_credit_model` over every interleaving with credit,
//! shrink and return frames reordered and duplicated.

use std::ops::Index;
use std::ops::IndexMut;

use serde::Deserialize;
use serde::Serialize;

use crate::core::pool::ChannelLane;
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;

/// Frames of one lane a receiver holds at most; a connection's bound, one window per lane, is
/// [`INBOUND_PEER_FRAME_CAPACITY`](crate::callback::INBOUND_PEER_FRAME_CAPACITY).
pub const LANE_CREDIT_WINDOW: u64 = 16;

/// Released frames a receiver accumulates before it advertises more credit: half a window, so
/// a sender that used its whole window is unblocked once half of it has been consumed, at one
/// credit frame per half window.
const LANE_CREDIT_BATCH: u64 = LANE_CREDIT_WINDOW / 2;

const _: () = assert!(0 < LANE_CREDIT_BATCH && LANE_CREDIT_BATCH <= LANE_CREDIT_WINDOW);

/// The frames of one data lane any receiver always grants: the first frame of a burst never
/// waits a round trip (load-bounds paper, Law 3).
const DATA_LANE_FLOOR: u64 = 1;

/// The credit window of one lane: the frames a receiver holds at most (its cap `W`), the frames
/// it always grants (its floor `g`), and the releases it accumulates before it advertises more.
///
/// Invariant: `0 < floor ≤ frames` and `0 < batch`. The batch a lane advertises at is taken of
/// what it is granted now, `floor + lent` (see `batch_of`), so a lane granted its floor alone
/// advertises each release: the progress law needs the batch no greater than the grant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CreditWindow {
    /// The cap `W`.
    frames: u64,
    /// The floor `g`.
    floor: u64,
    /// The advertising batch `b` of a whole window.
    batch: u64,
}

impl CreditWindow {
    /// The priority lane's window: [`LANE_CREDIT_WINDOW`] frames, all of them its floor, so it
    /// never borrows, advertised half a window at a time.
    pub(crate) const PRODUCTION: Self = Self {
        frames: LANE_CREDIT_WINDOW,
        floor: LANE_CREDIT_WINDOW,
        batch: LANE_CREDIT_BATCH,
    };

    /// A data lane's window: a floor of one frame, and up to [`LANE_CREDIT_WINDOW`] with the
    /// share the node's pool lends it.
    pub(crate) const POOLED: Self = Self {
        frames: LANE_CREDIT_WINDOW,
        floor: DATA_LANE_FLOOR,
        batch: LANE_CREDIT_BATCH,
    };

    /// The window of `lane`: the priority lane's whole window, or a data lane's pooled one.
    pub(crate) const fn of(index: CreditIndex) -> Self {
        match index {
            CreditIndex::C0 => Self::PRODUCTION,
            CreditIndex::C1 | CreditIndex::C2 | CreditIndex::C3 => Self::POOLED,
        }
    }

    /// A window of `frames` advertised `batch` at a time and never pooled, for models that
    /// explore a smaller state space than the production window.
    ///
    /// Pre: `0 < batch ≤ frames`.
    #[cfg(test)]
    pub(crate) const fn new(frames: u64, batch: u64) -> Self {
        assert!(0 < batch && batch <= frames);
        Self {
            frames,
            floor: frames,
            batch,
        }
    }

    /// A pooled window of cap `frames` and floor `floor`, advertised at most `batch` at a time.
    ///
    /// Pre: `0 < floor ≤ frames`, `0 < batch`.
    #[cfg(test)]
    pub(crate) const fn pooled(frames: u64, floor: u64, batch: u64) -> Self {
        assert!(0 < floor && floor <= frames && 0 < batch);
        Self {
            frames,
            floor,
            batch,
        }
    }

    /// The cap `W`, for the model.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) const fn frames(self) -> u64 {
        self.frames
    }

    /// The batch of a lane granted `grant` frames: half of it, at least one, at most `b`.
    const fn batch_of(self, grant: u64) -> u64 {
        let half = grant / 2;
        let batch = if half < self.batch { half } else { self.batch };
        if batch == 0 {
            1
        } else {
            batch
        }
    }
}

/// One credit signal of a lane, which the receiver sends to the sender (a credit or a shrink)
/// or the sender to the receiver (a return): never charged against credit, and idempotent per
/// generation, so a link may duplicate and reorder them (load-bounds paper, Def. Reclaim).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) enum LaneSignal {
    /// `Credit(A, g)`: the sender may commit the lane's frames with index below `A`.
    Credit {
        /// `A`.
        limit: u64,
        /// `g`.
        generation: u64,
    },
    /// `Shrink(L′, g)`: from generation `g`, the sender's limit is `L′`.
    Shrink {
        /// `L′`.
        limit: u64,
        /// `g`.
        generation: u64,
    },
    /// `Returned(B, g)`: the sender commits at most `B` from generation `g` on.
    Returned {
        /// `B`.
        bound: u64,
        /// `g`.
        generation: u64,
    },
}

/// The index a lane's credit is kept under: the channel the lane is pinned to, one of the
/// [`DATA_CHANNEL_POOL_SIZE`] channels by construction.
///
/// Lanes congruent modulo [`DATA_CHANNEL_POOL_SIZE`] share one channel and so one credit,
/// exactly as they share its ordering (`channel(lane) ≜ pool[lane mod |pool|]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditIndex {
    /// Channel 0.
    C0,
    /// Channel 1.
    C1,
    /// Channel 2.
    C2,
    /// Channel 3.
    C3,
}

const _: () = assert!(
    DATA_CHANNEL_POOL_SIZE == 4,
    "CreditIndex and PerLane name one variant and one value per data channel"
);

impl CreditIndex {
    /// Every index, in channel order.
    pub(crate) const ALL: [Self; 4] = [Self::C0, Self::C1, Self::C2, Self::C3];

    /// The least lane of the channel, which credit frames are sent on, logs name, and an
    /// admitted frame reports as the lane it arrived on.
    pub(crate) const fn lane(self) -> ChannelLane {
        ChannelLane::new(self as u8)
    }
}

/// The credit index of `lane`.
pub(crate) const fn credit_index(lane: ChannelLane) -> CreditIndex {
    match lane.index() % DATA_CHANNEL_POOL_SIZE {
        0 => CreditIndex::C0,
        1 => CreditIndex::C1,
        2 => CreditIndex::C2,
        _ => CreditIndex::C3,
    }
}

/// One value per credit index; selecting by a [`CreditIndex`] is total.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PerLane<T>([T; 4]);

impl<T> PerLane<T> {
    /// The values `value(index)` of every index.
    pub(crate) fn from_fn(value: impl FnMut(CreditIndex) -> T) -> Self {
        Self(CreditIndex::ALL.map(value))
    }

    /// Every index with its value, in channel order.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (CreditIndex, &mut T)> {
        CreditIndex::ALL.into_iter().zip(self.0.iter_mut())
    }
}

impl<T> Index<CreditIndex> for PerLane<T> {
    type Output = T;

    fn index(&self, index: CreditIndex) -> &T {
        let [c0, c1, c2, c3] = &self.0;
        match index {
            CreditIndex::C0 => c0,
            CreditIndex::C1 => c1,
            CreditIndex::C2 => c2,
            CreditIndex::C3 => c3,
        }
    }
}

impl<T> IndexMut<CreditIndex> for PerLane<T> {
    fn index_mut(&mut self, index: CreditIndex) -> &mut T {
        let [c0, c1, c2, c3] = &mut self.0;
        match index {
            CreditIndex::C0 => c0,
            CreditIndex::C1 => c1,
            CreditIndex::C2 => c2,
            CreditIndex::C3 => c3,
        }
    }
}

/// The sending end of one lane.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SendCredit {
    /// Frames whose sends became irrevocable.
    committed: u64,
    /// Sends holding a credit that are not yet irrevocable.
    reserved: u64,
    /// The greatest credit received in the current generation, or the bound returned for it.
    limit: u64,
    /// The current generation `g`: the last shrink applied.
    generation: u64,
    /// The bound returned for the current generation's shrink, sent again for a duplicate.
    returned: Option<u64>,
}

impl SendCredit {
    /// The sender of a fresh connection generation: the window's floor granted.
    pub(crate) const fn new(window: CreditWindow) -> Self {
        Self {
            committed: 0,
            reserved: 0,
            limit: window.floor,
            generation: 0,
            returned: None,
        }
    }

    #[cfg(any(test, rings_transport_backend))]
    /// Reserve one credit for a send, if one is free.
    ///
    /// Post: on `true`, `committed + reserved ≤ limit` still holds with one more reservation.
    pub(crate) fn try_reserve(&mut self) -> bool {
        let free = self.free() > 0;
        if free {
            self.reserved = self.reserved.saturating_add(1);
        }
        free
    }

    #[cfg(any(test, rings_transport_backend))]
    /// Turn one reservation into a sent frame: the send became irrevocable.
    ///
    /// Pre: a reservation is held.
    pub(crate) fn commit(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
        self.committed = self.committed.saturating_add(1);
    }

    #[cfg(any(test, rings_transport_backend))]
    /// Return one reservation: the send was abandoned before it became irrevocable.
    ///
    /// Pre: a reservation is held.
    pub(crate) fn cancel(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
    }

    /// The credits free for a new reservation: `limit − (committed + reserved)`, saturating.
    pub(crate) const fn free(self) -> u64 {
        self.limit
            .saturating_sub(self.committed.saturating_add(self.reserved))
    }

    /// Join a received credit of `generation`: `limit ← max(limit, credit)` in the sender's own
    /// generation; a credit of another generation changes nothing.
    pub(crate) fn grant(&mut self, limit: u64, generation: u64) {
        if generation == self.generation {
            self.limit = self.limit.max(limit);
        }
    }

    /// Apply a shrink to `limit` from `generation`: commit at most `B = max(c, limit)` from now
    /// on, `c` being the frames committed or reserved, and return `B` for the receiver, which
    /// counts exactly `B`; so `B` is also the new limit, even above a credit still in transit,
    /// which belongs to the older generation. A duplicate of the current generation's shrink
    /// returns the same bound again; a shrink of an older generation returns nothing.
    pub(crate) fn shrink(&mut self, limit: u64, generation: u64) -> Option<u64> {
        if generation > self.generation {
            let bound = self.committed.saturating_add(self.reserved).max(limit);
            self.limit = bound;
            self.generation = generation;
            self.returned = Some(bound);
        }
        self.returned.filter(|_| generation == self.generation)
    }

    #[cfg(rings_transport_backend)]
    /// The frames sent and the credit they were sent under, for diagnostics.
    pub(crate) const fn usage(self) -> (u64, u64) {
        (self.committed, self.limit)
    }
}

/// A frame that arrived beyond the credit its receiver advertised: the sender broke the
/// protocol, since an honest sender never exceeds a credit it was granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CreditViolation {
    /// Frames of the lane already received.
    pub(crate) received: u64,
    /// The credit advertised for the lane.
    pub(crate) advertised: u64,
}

/// The receiving end of one lane.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ReceiveWindow {
    /// Frames admitted.
    received: u64,
    /// Frames that left the transport.
    released: u64,
    /// The greatest credit advertised, the initial floor included, or the bound a return set.
    advertised: u64,
    /// The share of the node's pool the lane holds beyond its floor.
    lent: u64,
    /// The current generation `g`.
    generation: u64,
    /// The limit of the current generation's shrink, until its return arrives.
    shrinking: Option<u64>,
    /// Whether a frame was admitted since the last idle check.
    active: bool,
    /// Whether the pool lent the lane more since its last advertisement, which is then due at
    /// once rather than at the next batch of releases.
    grown: bool,
}

impl ReceiveWindow {
    /// The receiver of a fresh connection generation: the window's floor granted.
    pub(crate) const fn new(window: CreditWindow) -> Self {
        Self {
            received: 0,
            released: 0,
            advertised: window.floor,
            lent: 0,
            generation: 0,
            shrinking: None,
            active: false,
            grown: false,
        }
    }

    /// Admit one arriving frame of the lane, or report the credit it exceeded.
    ///
    /// Post: on `Ok`, `received ≤ advertised`; an `Err` leaves the window unchanged.
    pub(crate) fn admit(&mut self) -> Result<(), CreditViolation> {
        if self.received < self.advertised {
            self.received = self.received.saturating_add(1);
            self.active = true;
            Ok(())
        } else {
            Err(CreditViolation {
                received: self.received,
                advertised: self.advertised,
            })
        }
    }

    /// Release one admitted frame: it left the transport.
    ///
    /// Pre: `released < received`.
    pub(crate) fn release(&mut self) {
        self.released = self.released.saturating_add(1).min(self.received);
    }

    /// Return the credit to advertise, with its generation, if the frames released since the
    /// last advertisement complete a batch of what the lane is granted, or the pool lent it more
    /// since; nothing while a shrink is pending.
    ///
    /// Post: on `Some((limit, g))`, `advertised = limit = released + floor + lent ≤
    /// released + W`; on `None`, nothing changed.
    pub(crate) fn advertise(&mut self, window: CreditWindow) -> Option<(u64, u64)> {
        if self.shrinking.is_some() {
            return None;
        }
        let grant = window.floor.saturating_add(self.lent);
        let target = self.released.saturating_add(grant);
        let batch_complete = target >= self.advertised.saturating_add(window.batch_of(grant));
        let due = batch_complete || (self.grown && target > self.advertised);
        due.then(|| {
            self.advertised = target;
            self.grown = false;
            (target, self.generation)
        })
    }

    /// The share of the pool the lane may still borrow: `W − floor − lent`, none while a shrink
    /// is pending.
    pub(crate) fn want(&self, window: CreditWindow) -> u64 {
        match self.shrinking {
            Some(_) => 0,
            None => window
                .frames
                .saturating_sub(window.floor)
                .saturating_sub(self.lent),
        }
    }

    /// Take `share` more of the pool, at most what the lane wants; returns what it took.
    pub(crate) fn lend(&mut self, window: CreditWindow, share: u64) -> u64 {
        let taken = share.min(self.want(window));
        self.lent = self.lent.saturating_add(taken);
        self.grown |= taken > 0;
        taken
    }

    /// What the lane holds of the pool: its share, or, while a shrink is pending or its frames
    /// drain, the liability it still counts beyond its floor, whichever is greater.
    pub(crate) fn charge(&self, window: CreditWindow) -> u64 {
        let liability = self.advertised.saturating_sub(self.released);
        self.lent.max(liability.saturating_sub(window.floor))
    }

    #[cfg(any(test, feature = "dummy", rings_transport_webrtc))]
    /// The idle check: a lane that holds a share of the pool and admitted no frame since the
    /// last check is shrunk to its floor, in a new generation; returns the `Shrink` to send.
    pub(crate) fn idle_shrink(&mut self, window: CreditWindow) -> Option<LaneSignal> {
        let idle = !std::mem::replace(&mut self.active, false);
        if !idle {
            return None;
        }
        self.shrink_to(window, 0)
    }

    /// Shrink the lane's share of the pool to `lent`, in a new generation; returns the `Shrink`
    /// to send. Nothing while a shrink is pending, or when the share is no greater than `lent`.
    pub(crate) fn shrink_to(&mut self, window: CreditWindow, lent: u64) -> Option<LaneSignal> {
        if self.lent <= lent || self.shrinking.is_some() {
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        self.lent = lent;
        // Never above the advertised credit, so the bound the sender returns is not either.
        let limit = self
            .released
            .saturating_add(window.floor)
            .saturating_add(lent)
            .min(self.advertised);
        self.shrinking = Some(limit);
        Some(LaneSignal::Shrink {
            limit,
            generation: self.generation,
        })
    }

    /// The lane's share of the pool beyond its floor.
    pub(crate) const fn lent(&self) -> u64 {
        self.lent
    }

    /// Apply the sender's return of `bound` for `generation`: the shrink ends, and the lane
    /// counts what the sender may still commit, never more than it advertised nor less than it
    /// received. Returns whether it ended a pending shrink.
    pub(crate) fn returned(&mut self, bound: u64, generation: u64) -> bool {
        if self.shrinking.is_none() || generation != self.generation {
            return false;
        }
        self.advertised = bound.clamp(self.received, self.advertised);
        self.shrinking = None;
        true
    }

    /// The connection is gone: no frame will arrive, so the lane counts only what it holds.
    pub(crate) fn close(&mut self) {
        self.advertised = self.received;
        self.lent = 0;
        self.shrinking = None;
    }

    /// The credit the sender holds unused, as the receiver counts it: frames advertised and not
    /// yet arrived. A frame the receiver still holds, a release awaiting its batch, or a share
    /// the pool has not lent is not in it. Credit still in transit to the sender, and frames
    /// already sent on it that are still in flight, are: with the frames held, the lane's
    /// outstanding liability, which the window bounds by `W` however long the round trip.
    pub(crate) const fn unused(self) -> u64 {
        self.advertised.saturating_sub(self.received)
    }

    /// Frames admitted and not yet released.
    #[cfg(test)]
    pub(crate) const fn occupancy(self) -> u64 {
        self.received.saturating_sub(self.released)
    }

    /// The counted liability, `advertised − released`, for the model.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) const fn liability(self) -> u64 {
        self.advertised.saturating_sub(self.released)
    }
}

#[cfg(test)]
mod test_credit;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_credit_model;
