//! The receive side of one TCP relay direction: segments reordered by `seq` into the prefix of
//! the peer's byte stream, and the verdict that a gap can no longer be filled.
//!
//! ```text
//!   state  (next, ahead : seq ⇀ bytes, fin : Option sent)
//!   data(seq, b) | seq = next            ↦ write b, then every contiguous ahead[next..]
//!                | next < seq < next + W ↦ ahead[seq] := b                (a gap is open)
//!                | otherwise             ↦ Fault
//!   fin(sent)    | sent ≥ next ∧ sent > max ahead ∧ fin = None  ↦ fin := sent
//!                | otherwise             ↦ Fault
//!   finished     ⇔ fin = Some(next)
//!   gap          = Some next  if ¬finished ∧ (ahead ≠ ∅ ∨ fin ≠ None),  else None
//! ```
//!
//! Laws:
//! - **Prefix.** What has been written is exactly segments `0 .. next` in order, so the local
//!   stream is always a prefix of what the peer sent; a `Fault` never writes.
//! - **Bounded.** `ahead` holds fewer than `W` = [`REORDER_WINDOW`] segments: a segment that
//!   many behind a later one is refused by the replay window and cannot arrive.
//! - **Clean end only when complete.** `finished` holds only once every one of the `sent`
//!   segments was written.

use std::collections::BTreeMap;

use bytes::Bytes;
use rings_core::message::REORDER_WINDOW;

/// Why a direction's stream cannot continue: the session is reset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StreamFault {
    /// A segment at or past the window: the segment `next` can no longer arrive.
    Overrun {
        /// The segment still missing.
        next: u64,
        /// The segment that arrived.
        seq: u64,
    },
    /// A segment already written or already waiting: the peer repeated itself.
    Repeated {
        /// The repeated segment.
        seq: u64,
    },
    /// A segment at or past the half-close's count.
    PastFin {
        /// The segment that arrived.
        seq: u64,
        /// The half-close's count.
        sent: u64,
    },
    /// A half-close whose count is below a segment already seen, or a second half-close.
    BadFin {
        /// The half-close's count.
        sent: u64,
    },
}

/// The segments that became writable, in order, and whether the direction is now complete.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct Ready {
    /// Bytes to write to the local socket, in order.
    pub(super) segments: Vec<Bytes>,
    /// Whether every segment the half-close counted has been written: shut the write side.
    pub(super) finished: bool,
}

/// One direction's reorder state; see the module laws.
#[derive(Debug, Default)]
pub(super) struct StreamReorder {
    next: u64,
    ahead: BTreeMap<u64, Bytes>,
    fin: Option<u64>,
}

impl StreamReorder {
    /// Accept segment `seq` carrying `bytes`.
    pub(super) fn data(&mut self, seq: u64, bytes: Bytes) -> Result<Ready, StreamFault> {
        if let Some(sent) = self.fin.filter(|sent| seq >= *sent) {
            return Err(StreamFault::PastFin { seq, sent });
        }
        if seq < self.next || self.ahead.contains_key(&seq) {
            return Err(StreamFault::Repeated { seq });
        }
        if seq - self.next >= REORDER_WINDOW {
            return Err(StreamFault::Overrun {
                next: self.next,
                seq,
            });
        }
        self.ahead.insert(seq, bytes);
        Ok(self.release())
    }

    /// Accept the half-close counting `sent` segments.
    pub(super) fn fin(&mut self, sent: u64) -> Result<Ready, StreamFault> {
        let beyond_seen = self
            .ahead
            .last_key_value()
            .map_or(self.next, |(seq, _)| seq + 1);
        if self.fin.is_some() || sent < beyond_seen {
            return Err(StreamFault::BadFin { sent });
        }
        self.fin = Some(sent);
        Ok(self.release())
    }

    /// The segment missing although a later one, or the half-close, shows it was sent: the
    /// open gap, identified by the segment it waits for.
    pub(super) fn gap(&self) -> Option<u64> {
        let shown_sent = !self.ahead.is_empty() || self.fin.is_some();
        (!self.finished() && shown_sent).then_some(self.next)
    }

    /// Whether every segment the half-close counted has been written.
    fn finished(&self) -> bool {
        self.fin == Some(self.next)
    }

    /// Move the contiguous segments from `next` on out of `ahead`.
    fn release(&mut self) -> Ready {
        let mut segments = Vec::new();
        while let Some(bytes) = self.ahead.remove(&self.next) {
            segments.push(bytes);
            self.next += 1;
        }
        Ready {
            segments,
            finished: self.finished(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of segment `seq`: its own number, so a misordered write shows.
    fn segment(seq: u64) -> Bytes {
        Bytes::from(seq.to_be_bytes().to_vec())
    }

    /// The release of segments `seqs`, in that order, with the direction `finished` or not.
    fn ready(seqs: &[u64], finished: bool) -> Ready {
        Ready {
            segments: seqs.iter().copied().map(segment).collect(),
            finished,
        }
    }

    /// Segments in order are written as they arrive, and the half-close counting them ends the
    /// direction with no gap.
    #[test]
    fn test_in_order_segments_are_written_at_once() {
        let mut stream = StreamReorder::default();
        assert_eq!(stream.data(0, segment(0)), Ok(ready(&[0], false)));
        assert_eq!(stream.data(1, segment(1)), Ok(ready(&[1], false)));
        assert_eq!(stream.fin(2), Ok(ready(&[], true)));
        assert_eq!(stream.gap(), None);
    }

    /// Segments ahead of a missing one wait, open a gap at it, and are released in order once it
    /// arrives.
    #[test]
    fn test_a_reordered_segment_releases_the_ones_behind_it() {
        let mut stream = StreamReorder::default();
        assert_eq!(stream.data(1, segment(1)), Ok(ready(&[], false)));
        assert_eq!(stream.data(2, segment(2)), Ok(ready(&[], false)));
        assert_eq!(stream.gap(), Some(0));
        assert_eq!(stream.data(0, segment(0)), Ok(ready(&[0, 1, 2], false)));
        assert_eq!(stream.gap(), None);
    }

    /// A half-close that overtook a segment opens a gap at it, and ends the direction once it
    /// arrives.
    #[test]
    fn test_a_half_close_waits_for_the_segments_it_counts() {
        let mut stream = StreamReorder::default();
        assert_eq!(stream.data(0, segment(0)), Ok(ready(&[0], false)));
        // The half-close overtook segment 1: a gap is open until it arrives.
        assert_eq!(stream.fin(2), Ok(ready(&[], false)));
        assert_eq!(stream.gap(), Some(1));
        assert_eq!(stream.data(1, segment(1)), Ok(ready(&[1], true)));
        assert_eq!(stream.gap(), None);
    }

    /// A lost first, middle or last segment leaves the gap open at that segment, so the gap
    /// deadline resets the stream; nothing past the gap is ever written.
    #[test]
    fn test_a_lost_segment_leaves_its_gap_open() {
        for lost in [0, 2, 4] {
            let mut stream = StreamReorder::default();
            let mut written = Vec::new();
            for seq in (0..5).filter(|seq| *seq != lost) {
                let Ok(ready) = stream.data(seq, segment(seq)) else {
                    panic!("segment {seq} is within the window");
                };
                written.extend(ready.segments);
            }
            let Ok(ready) = stream.fin(5) else {
                panic!("the half-close counts every segment");
            };
            assert!(!ready.finished, "lost {lost}: never a clean end");
            assert_eq!(stream.gap(), Some(lost), "lost {lost}: the gap stays open");
            let prefix: Vec<Bytes> = (0..lost).map(segment).collect();
            assert_eq!(written, prefix, "lost {lost}: only the prefix is written");
        }
    }

    /// A segment a whole window ahead is a fault, one short of it waits: the window bound is
    /// exact.
    #[test]
    fn test_a_segment_past_the_window_is_a_fault() {
        let mut stream = StreamReorder::default();
        assert_eq!(
            stream.data(REORDER_WINDOW, segment(REORDER_WINDOW)),
            Err(StreamFault::Overrun {
                next: 0,
                seq: REORDER_WINDOW
            })
        );
        assert_eq!(
            stream.data(REORDER_WINDOW - 1, segment(REORDER_WINDOW - 1)),
            Ok(ready(&[], false))
        );
    }

    /// A repeated segment, a half-close below a segment seen or after another, and a segment
    /// past the half-close's count are faults, never writes.
    #[test]
    fn test_repeats_and_counts_that_contradict_the_stream_are_faults() {
        let mut stream = StreamReorder::default();
        assert_eq!(stream.data(0, segment(0)), Ok(ready(&[0], false)));
        assert_eq!(
            stream.data(0, segment(0)),
            Err(StreamFault::Repeated { seq: 0 })
        );
        assert_eq!(stream.data(2, segment(2)), Ok(ready(&[], false)));
        assert_eq!(
            stream.data(2, segment(2)),
            Err(StreamFault::Repeated { seq: 2 })
        );
        assert_eq!(stream.fin(2), Err(StreamFault::BadFin { sent: 2 }));
        assert_eq!(stream.fin(3), Ok(ready(&[], false)));
        assert_eq!(stream.fin(3), Err(StreamFault::BadFin { sent: 3 }));
        assert_eq!(
            stream.data(3, segment(3)),
            Err(StreamFault::PastFin { seq: 3, sent: 3 })
        );
    }
}
