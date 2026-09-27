//! The reorder window of one session direction (#834 D2′, Q7 of #843).
//!
//! Frames of one direction carry the sequence `n = 0, 1, 2, …` inside the carry. Loops travel
//! independently, so they may arrive out of order or not at all. The receiver keeps
//!
//! ```text
//! state = (next, P)       next: the least sequence not yet released,  P: n ↦ (frame, arrival)
//!
//! accept(n, f, t):
//!   n < next                       ─▶ drop (a late duplicate; L9 already refused a replay)
//!   n ≥ next + W                   ─▶ Gap (the window cannot hold it)
//!   otherwise P[n] ← (f, t);  release P[next], P[next + 1], … while present
//! expire(t):
//!   P ≠ ∅ ∧ t − min arrival(P) ≥ V ─▶ Gap (next is missing for V since a later frame came)
//! ```
//!
//! with the window `W = Q_max` frames and `V` the loop validity. There is no retransmission, so a
//! gap is permanent and fails the session closed: a lost loop never removes stream bytes silently.
//!
//! Laws (tested in `session::tests`):
//!
//! - **Order.** Released frames are exactly `n = 0, 1, 2, …` in order, each at most once.
//! - **Reordering.** Any permutation of `0 … m` with `m < W` releases `0 … m` in order.
//! - **Gap.** A frame `n ≥ next + W`, or a missing `next` for `V` after a later frame arrived,
//!   yields [`OnionSequenceGap`]; after it the direction releases nothing more.

use std::collections::BTreeMap;

use super::frame::OnionSequence;
use super::pool::ONION_SURB_POOL_CAPACITY;
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;

/// The reorder window `W = Q_max`, in frames.
const REORDER_WINDOW: u32 = ONION_SURB_POOL_CAPACITY as u32;

/// A direction's sequence has a gap it can no longer fill: the session fails closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("session sequence gap: frame {missing} never arrived")]
pub(crate) struct OnionSequenceGap {
    /// The first sequence that did not arrive.
    pub(crate) missing: u32,
}

/// The reorder state of one direction; see the module documentation.
#[derive(Debug)]
pub(crate) struct OnionReorder<F> {
    /// The least sequence not yet released.
    next: OnionSequence,
    /// Frames received ahead of `next`, with their arrival instants.
    pending: BTreeMap<OnionSequence, (F, u128)>,
    /// Set once a gap is detected; the direction then releases nothing.
    failed: Option<OnionSequenceGap>,
    /// Set once the frame at `u32::MAX` is released: the direction takes no further frame,
    /// but it has no gap, so only a further frame fails it.
    exhausted: bool,
}

impl<F> Default for OnionReorder<F> {
    /// A direction at its first sequence, with nothing pending.
    fn default() -> Self {
        Self {
            next: OnionSequence::FIRST,
            pending: BTreeMap::new(),
            failed: None,
            exhausted: false,
        }
    }
}

impl<F> OnionReorder<F> {
    /// Accept frame `sequence` at `now`, returning the frames it releases, in order.
    ///
    /// # Errors
    ///
    /// [`OnionSequenceGap`] if the frame lies beyond the window or the direction has already
    /// failed.
    pub(crate) fn accept(
        &mut self,
        now_ms: u128,
        sequence: OnionSequence,
        frame: F,
    ) -> Result<Vec<F>, OnionSequenceGap> {
        self.expire(now_ms)?;
        if self.exhausted {
            return Err(self.fail());
        }
        if sequence < self.next {
            return Ok(Vec::new());
        }
        if sequence.value() - self.next.value() >= REORDER_WINDOW {
            return Err(self.fail());
        }
        self.pending.entry(sequence).or_insert((frame, now_ms));
        let mut released = Vec::new();
        while let Some((frame, _)) = self.pending.remove(&self.next) {
            released.push(frame);
            match self.next.next() {
                Some(next) => self.next = next,
                // The last sequence is released; the direction can take nothing more, so the
                // next frame fails it, but what was released stands.
                None => {
                    self.pending.clear();
                    self.exhausted = true;
                    break;
                }
            }
        }
        Ok(released)
    }

    /// Start the direction at `next`, for tests at the end of the sequence space.
    #[cfg(test)]
    pub(crate) fn next_for_test(&mut self, next: OnionSequence) {
        self.next = next;
    }

    /// Fail closed if the missing `next` has been awaited for `V` since a later frame arrived.
    ///
    /// # Errors
    ///
    /// [`OnionSequenceGap`] then, and whenever the direction has already failed.
    pub(crate) fn expire(&mut self, now_ms: u128) -> Result<(), OnionSequenceGap> {
        if let Some(gap) = self.failed {
            return Err(gap);
        }
        let stalled = self
            .pending
            .values()
            .map(|(_, arrival)| *arrival)
            .min()
            .is_some_and(|arrival| now_ms.saturating_sub(arrival) >= ONION_FORWARD_MAX_VALIDITY_MS);
        if stalled {
            return Err(self.fail());
        }
        Ok(())
    }

    /// Record the gap at `next` and drop every pending frame.
    fn fail(&mut self) -> OnionSequenceGap {
        let gap = OnionSequenceGap {
            missing: self.next.value(),
        };
        self.pending.clear();
        self.failed = Some(gap);
        gap
    }
}
