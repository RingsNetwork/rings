//! The reorder window of one session direction (#834 D2′, Q7 of #843).
//!
//! Frames of one direction carry the sequence `n = 0, 1, 2, …` inside the carry. Loops travel
//! independently, so they may arrive out of order or not at all. The receiver keeps
//!
//! ```text
//! state = (next, P, M)    next: the least sequence not yet released,  P: n ↦ (frame, arrival)
//!                         M = (m₁, a₁) … (m_j, a_j): high-water marks above next, with
//!                             next < m₁ < … < m_j and their arrivals a_i
//!
//! accept(n, f, t):
//!   n < next                       ─▶ drop (a late duplicate; L9 already refused a replay)
//!   n ≥ next + W                   ─▶ Gap (the window cannot hold it)
//!   otherwise P[n] ← (f, t);  release P[next], P[next + 1], … while present;
//!             drop every (m, a) ∈ M with m ≤ next
//! mark(m, t):
//!   m ≤ next ∨ m ≤ m_j             ─▶ nothing (an earlier mark at least m already stands)
//!   otherwise M ← M ‖ (m, t)
//! missing = min({a | (_, a) ∈ P} ∪ {a | (_, a) ∈ M})    since when next is known missing
//! expire(t):
//!   missing defined ∧ t − missing ≥ V ─▶ Gap (next is missing for V since a later frame,
//!                                             or a mark above it, came)
//! ```
//!
//! with the window `W = Q_max` frames and `V` the loop validity. There is no retransmission, so a
//! gap is permanent and fails the session closed: a lost loop never removes stream bytes silently.
//!
//! A mark `m` is the sender's next sequence when it sent the mark, carried by an unsequenced
//! frame (`credit(m, υ…)`, paper D2′ Order; #981): every frame below `m` was sent, so a missing
//! `next < m` is known exactly as when a later frame arrives, and the last frame of a direction
//! is exposed by the next mark even when no later frame follows it. A frame below a mark that
//! arrived at `a` was sent before `a`, so it arrives by `a + V` if at all: no frame that can
//! still arrive is failed. A mark at most an earlier one adds nothing, since the earlier one
//! arrived first and already witnesses every frame below it. `M` holds only marks of the last
//! `V`, since an arrival older than `V` fails the direction, so it grows by at most one entry per
//! arrival of that window.
//!
//! Laws (tested in `session::tests`):
//!
//! - **Order.** Released frames are exactly `n = 0, 1, 2, …` in order, each at most once.
//! - **Reordering.** Any permutation of `0 … m` with `m < W` releases `0 … m` in order.
//! - **Gap.** A frame `n ≥ next + W`, or a missing `next` for `V` after a later frame or a mark
//!   above it arrived, yields [`OnionSequenceGap`]; after it the direction releases nothing more.
//! - **Mark.** A mark never releases a frame nor occupies the window: `mark` changes only `M`,
//!   and a mark `m ≤ next` changes nothing.
//! - **Stall.** `stalled ⇔ (P ≠ ∅ ∨ M ≠ ∅) ∧ ¬failed`: from a later frame's or a mark's
//!   arrival until `next` arrives or the gap fails the direction, the receiver knows a frame is
//!   missing ([`OnionReorder::is_stalled`]).

use std::collections::BTreeMap;
use std::collections::VecDeque;

use super::frame::OnionSequence;
use super::pool::ONION_SURB_POOL_CAPACITY;
use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;

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
    /// `M`: the marks above `next` that no earlier mark dominates, increasing, with their arrival
    /// instants.
    marks: VecDeque<(OnionSequence, u128)>,
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
            marks: VecDeque::new(),
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
        self.discharge_marks();
        Ok(released)
    }

    /// Accept the high-water mark `mark` at `now`: every frame below it was sent. A mark above
    /// `next` and above every mark held is kept, so the missing `next` is known from `now`; any
    /// other mark changes nothing (Law Mark).
    ///
    /// # Errors
    ///
    /// [`OnionSequenceGap`] if the direction has already failed, or fails at `now`.
    pub(crate) fn mark(
        &mut self,
        now_ms: u128,
        mark: OnionSequence,
    ) -> Result<(), OnionSequenceGap> {
        self.expire(now_ms)?;
        // `M` is pruned to marks above `next`, so its last mark, else `next`, is the floor.
        let floor = self.marks.back().map_or(self.next, |(highest, _)| *highest);
        if mark > floor {
            self.marks.push_back((mark, now_ms));
        }
        Ok(())
    }

    /// Whether a frame is known missing: a later one arrived and is held behind `next`, or a mark
    /// above `next` arrived, and the direction has not failed. It holds from such an arrival
    /// until `next` arrives or [`Self::expire`] fails the direction, so it is the observable half
    /// of a gap (#950).
    pub(crate) fn is_stalled(&self) -> bool {
        self.failed.is_none() && (!self.pending.is_empty() || !self.marks.is_empty())
    }

    /// Start the direction at `next`, for tests at the end of the sequence space.
    #[cfg(test)]
    pub(crate) fn next_for_test(&mut self, next: OnionSequence) {
        self.next = next;
    }

    /// Fail closed if the missing `next` has been awaited for `V` since a later frame, or a
    /// mark above it, arrived.
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
            .chain(self.marks.iter().map(|(_, arrival)| *arrival))
            .min()
            .is_some_and(|arrival| now_ms.saturating_sub(arrival) >= ONION_ADMISSION_WINDOW_MS);
        if stalled {
            return Err(self.fail());
        }
        Ok(())
    }

    /// Drop the marks the released frames reached, `m ≤ next`: they no longer witness a missing
    /// frame. The marks increase, so they are a prefix of `M`.
    fn discharge_marks(&mut self) {
        while self
            .marks
            .front()
            .is_some_and(|(mark, _)| *mark <= self.next)
        {
            self.marks.pop_front();
        }
    }

    /// Record the gap at `next` and drop every pending frame and mark.
    fn fail(&mut self) -> OnionSequenceGap {
        let gap = OnionSequenceGap {
            missing: self.next.value(),
        };
        self.pending.clear();
        self.marks.clear();
        self.failed = Some(gap);
        gap
    }
}
