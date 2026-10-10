//! Per-lane credit flow control: the receiver bounds how many frames of each lane it holds, and
//! the sender never exceeds that bound, so receive-side admission never refuses an honest frame.
//!
//! A connection carries [`DATA_CHANNEL_POOL_SIZE`] lanes, each pinned to one ordered data
//! channel (see [`crate::core::pool`]). For every lane, independently, the two ends keep:
//!
//! ```text
//!   sender   S = (committed, reserved, limit)        limit    : the greatest credit received
//!   receiver R = (received, released, advertised)    advertised: the greatest credit granted
//!
//!   reserve  : S → S + 1 reservation      if committed + reserved < limit
//!   commit   : one reservation becomes one sent frame (the send became irrevocable)
//!   cancel   : one reservation is returned (the send was abandoned before it was irrevocable)
//!   grant(l) : limit ← max(limit, l)                       (a credit frame arrives)
//!   admit    : received ← received + 1    if received < advertised, otherwise a violation
//!   release  : released ← released + 1                    (the frame left the transport)
//!   advertise: advertised ← released + W  if it exceeds advertised by the batch b
//! ```
//!
//! A release is followed by an advertisement unless the node is under load: the shell defers a
//! lane's advertisement while the frames its connections hold together exceed a soft limit, and
//! advertises once they fall below it (see `callback::link_credit`). Deferring narrows the
//! window the sender sees; it never takes back a credit already advertised.
//!
//! Both counters start at the window `W`: a fresh connection generation grants `W` frames per
//! lane without any credit frame. Credits are cumulative frame indices, so `grant` is the join
//! of the max-semilattice `(ℕ, max)`: idempotent, commutative and associative. A duplicated,
//! stale or reordered credit frame therefore changes nothing a newer one has not already
//! changed, and the link may treat credit frames as datagrams.
//!
//! Laws, for an honest sender and every interleaving of sends, deliveries and releases:
//!
//! - **Bound.** `received − released ≤ W`: `received ≤ advertised` by admission, and every
//!   advertised value is `released + W` at the time it was advertised, with `released`
//!   monotone.
//! - **No honest violation.** Frames of a lane arrive in order and at most once, so the
//!   `n`-th arrival was the `n`-th commit; `committed ≤ limit ≤ advertised`, since every limit
//!   the sender holds was advertised. Hence `received < advertised` at every honest arrival.
//! - **Progress.** If the receiver releases every frame it admitted, every deferred
//!   advertisement is eventually made, and every credit frame is eventually delivered once, the
//!   sender is never blocked forever: when it has committed all it may
//!   (`committed = advertised`) and the receiver has released them all
//!   (`released = advertised`), the next advertisement is `advertised + W`.
//! - **Isolation.** The state of a connection is the product of its lanes' states, and no
//!   transition of one lane reads another, so one lane's backlog never blocks another lane.
//!
//! These laws are model-checked in `test_credit_model` over every interleaving with credits
//! reordered and duplicated.

use crate::core::pool::ChannelLane;
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;

/// Frames of one lane a receiver holds at most. A connection therefore holds at most
/// `DATA_CHANNEL_POOL_SIZE × LANE_CREDIT_WINDOW` frames (64), each of at most
/// `MAX_DATA_CHANNEL_MESSAGE_SIZE` bytes (4 MiB in all).
pub const LANE_CREDIT_WINDOW: u64 = 16;

/// Released frames a receiver accumulates before it advertises more credit: half a window, so
/// a sender that used its whole window is unblocked once half of it has been consumed, at one
/// credit frame per half window.
const LANE_CREDIT_BATCH: u64 = LANE_CREDIT_WINDOW / 2;

const _: () = assert!(0 < LANE_CREDIT_BATCH && LANE_CREDIT_BATCH <= LANE_CREDIT_WINDOW);

/// The credit window of one lane: the frames a receiver holds at most, and the releases it
/// accumulates before it advertises more.
///
/// Invariant: `0 < batch ≤ frames`; the progress law needs `batch ≤ frames`, so that releasing
/// a whole window always advertises.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CreditWindow {
    /// The window `W`.
    frames: u64,
    /// The advertising batch `b`.
    batch: u64,
}

impl CreditWindow {
    /// The production window: [`LANE_CREDIT_WINDOW`] frames, advertised half a window at a time.
    pub(crate) const PRODUCTION: Self = Self {
        frames: LANE_CREDIT_WINDOW,
        batch: LANE_CREDIT_BATCH,
    };

    /// A window of `frames` advertised `batch` at a time, for models that explore a smaller
    /// state space than the production window.
    ///
    /// Pre: `0 < batch ≤ frames`.
    #[cfg(test)]
    pub(crate) const fn new(frames: u64, batch: u64) -> Self {
        assert!(0 < batch && batch <= frames);
        Self { frames, batch }
    }

    /// The window `W`, for the model.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) const fn frames(self) -> u64 {
        self.frames
    }
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

    /// The least lane of the channel, which credit frames are sent on and logs name.
    #[cfg(rings_transport_backend)]
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

impl<T> std::ops::Index<CreditIndex> for PerLane<T> {
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

impl<T> std::ops::IndexMut<CreditIndex> for PerLane<T> {
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
    /// The greatest credit received.
    limit: u64,
}

impl SendCredit {
    /// The sender of a fresh connection generation: `W` frames granted.
    pub(crate) const fn new(window: CreditWindow) -> Self {
        Self {
            committed: 0,
            reserved: 0,
            limit: window.frames,
        }
    }

    #[cfg(any(test, rings_transport_backend))]
    /// Reserve one credit for a send, if one is free.
    ///
    /// Post: on `true`, `committed + reserved ≤ limit` still holds with one more reservation.
    pub(crate) fn try_reserve(&mut self) -> bool {
        let free = self
            .committed
            .checked_add(self.reserved)
            .is_some_and(|used| used < self.limit);
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

    /// Join a received credit: `limit ← max(limit, credit)`.
    pub(crate) fn grant(&mut self, limit: u64) {
        self.limit = self.limit.max(limit);
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
    /// The greatest credit advertised, the initial window included.
    advertised: u64,
}

impl ReceiveWindow {
    /// The receiver of a fresh connection generation: `W` frames granted.
    pub(crate) const fn new(window: CreditWindow) -> Self {
        Self {
            received: 0,
            released: 0,
            advertised: window.frames,
        }
    }

    /// Admit one arriving frame of the lane, or report the credit it exceeded.
    ///
    /// Post: on `Ok`, `received ≤ advertised`; an `Err` leaves the window unchanged.
    pub(crate) fn admit(&mut self) -> Result<(), CreditViolation> {
        if self.received < self.advertised {
            self.received = self.received.saturating_add(1);
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

    /// Return the credit to advertise if the frames released since the last advertisement
    /// complete a batch.
    ///
    /// Post: on `Some(limit)`, `advertised = limit = released + W`; on `None`, nothing changed.
    pub(crate) fn advertise(&mut self, window: CreditWindow) -> Option<u64> {
        let target = self.released.saturating_add(window.frames);
        let batch_complete = target >= self.advertised.saturating_add(window.batch);
        batch_complete.then(|| {
            self.advertised = target;
            target
        })
    }

    /// Frames admitted and not yet released.
    #[cfg(test)]
    pub(crate) const fn occupancy(self) -> u64 {
        self.received.saturating_sub(self.released)
    }
}

#[cfg(test)]
mod test_credit;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_credit_model;
