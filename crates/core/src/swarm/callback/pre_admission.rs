//! Frames that arrive on a connection before this node has admitted it.
//!
//! Admission is asymmetric in time: each end admits an edge on its own data-channel-open
//! callback, so a peer that has admitted the edge may send over it before this end has. Such a
//! frame is authenticated (its transport edge and both signatures were verified before it got
//! here) and merely early, so it is held rather than dropped, and released once admission commits.
//!
//! A hold is a queue and a drain flag, with the arrival map
//! `arrive : Hold × F × Admitted → Arrival`:
//!
//! - admitted, nothing queued, and no drain in progress: `Pass(frame)`, nothing is ahead of it;
//! - otherwise the frame is appended behind everything already held, `Held`, or handed back as
//!   `Overflow` when the hold is full.
//!
//! Law (order): frames leave a hold in arrival order, and a frame that arrives while the hold
//! drains leaves after every frame held before it, including the one the drainer is delivering.
//! A drain is exclusive: `begin_drain` grants at most one drainer until it observes the empty
//! queue, so two callers cannot interleave. Law (bound): a hold keeps at most `capacity`
//! frames; the frame that would exceed it never enters the queue.

use std::collections::VecDeque;

/// The verdict on one arriving frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Arrival<F> {
    /// Nothing is ahead of the frame: deliver it now.
    Pass(F),
    /// The frame is queued behind admission or behind earlier held frames.
    Held,
    /// The hold is full; the frame is handed back undelivered.
    Overflow(F),
}

/// The frames held for one connection awaiting admission.
#[derive(Debug)]
pub(super) struct PreAdmissionHold<F> {
    queue: VecDeque<F>,
    /// One drainer is releasing held frames; arrivals queue behind them.
    draining: bool,
    capacity: usize,
}

impl<F> PreAdmissionHold<F> {
    /// An empty hold that keeps at most `capacity` frames.
    pub(super) const fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            draining: false,
            capacity,
        }
    }

    /// Judge one arriving frame given whether the connection is admitted now.
    pub(super) fn arrive(&mut self, frame: F, admitted: bool) -> Arrival<F> {
        if admitted && !self.draining && self.queue.is_empty() {
            return Arrival::Pass(frame);
        }
        if self.queue.len() >= self.capacity {
            return Arrival::Overflow(frame);
        }
        self.queue.push_back(frame);
        Arrival::Held
    }

    /// Claim the drain.
    ///
    /// Post: `true` implies the caller is the only drainer until [`Self::drain_next`] returns
    /// `None`; `false` means another drainer is active or there is nothing to drain.
    pub(super) fn begin_drain(&mut self) -> bool {
        if self.draining || self.queue.is_empty() {
            return false;
        }
        self.draining = true;
        true
    }

    /// The next frame to release, in arrival order; `None` ends the drain.
    ///
    /// Pre: the caller holds the drain granted by [`Self::begin_drain`].
    pub(super) fn drain_next(&mut self) -> Option<F> {
        let next = self.queue.pop_front();
        if next.is_none() {
            self.draining = false;
        }
        next
    }

    /// Forget every held frame: the connection will never be admitted.
    pub(super) fn discard(&mut self) -> usize {
        let discarded = self.queue.len();
        self.queue.clear();
        discarded
    }

    /// The frames currently held.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Law: before admission every arrival is held, in order, up to the bound.
    #[test]
    fn test_holds_in_arrival_order_up_to_capacity() {
        let mut hold = PreAdmissionHold::new(2);

        assert_eq!(hold.arrive(1, false), Arrival::Held);
        assert_eq!(hold.arrive(2, false), Arrival::Held);
        assert_eq!(hold.arrive(3, false), Arrival::Overflow(3));
        assert_eq!(hold.len(), 2);

        assert!(hold.begin_drain());
        assert_eq!(hold.drain_next(), Some(1));
        assert_eq!(hold.drain_next(), Some(2));
        assert_eq!(hold.drain_next(), None);
        assert_eq!(hold.arrive(4, true), Arrival::Pass(4));
    }

    /// Law: an admitted arrival passes only through an empty hold with no drain in progress;
    /// otherwise it queues behind what is held, so the order of arrival is the order of release.
    #[test]
    fn test_admitted_arrival_queues_behind_held_frames() {
        let mut hold = PreAdmissionHold::new(4);
        assert_eq!(hold.arrive(1, false), Arrival::Held);

        assert_eq!(hold.arrive(2, true), Arrival::Held);
        assert!(hold.begin_drain());
        assert_eq!(hold.drain_next(), Some(1));
        assert_eq!(hold.drain_next(), Some(2));
        // The drainer is delivering frame 2: a new arrival queues behind it.
        assert_eq!(hold.arrive(3, true), Arrival::Held);
        assert_eq!(hold.drain_next(), Some(3));
        assert_eq!(hold.drain_next(), None);
        assert_eq!(hold.arrive(4, true), Arrival::Pass(4));
    }

    /// Law: an admitted connection with nothing held passes frames from the start, and the
    /// drain is exclusive.
    #[test]
    fn test_admitted_empty_hold_passes_and_drain_is_exclusive() {
        let mut hold = PreAdmissionHold::new(4);
        assert!(!hold.begin_drain());
        assert_eq!(hold.arrive(1, true), Arrival::Pass(1));

        assert_eq!(hold.arrive(2, false), Arrival::Held);
        assert!(hold.begin_drain());
        assert!(!hold.begin_drain());
        assert_eq!(hold.drain_next(), Some(2));
        assert_eq!(hold.drain_next(), None);
        assert!(!hold.begin_drain());
    }

    /// Law: discarding forgets every held frame and reports how many.
    #[test]
    fn test_discard_forgets_held_frames() {
        let mut hold = PreAdmissionHold::new(4);
        assert_eq!(hold.arrive(1, false), Arrival::Held);
        assert_eq!(hold.arrive(2, false), Arrival::Held);

        assert_eq!(hold.discard(), 2);
        assert_eq!(hold.len(), 0);
        assert!(!hold.begin_drain());
    }
}
