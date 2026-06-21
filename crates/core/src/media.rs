#![warn(missing_docs)]
//! Receive-side media depacketization — the *media* sibling of [`crate::chunk`].
//!
//! Both are instances of the same [`ReassemblyStrategy`] / [`Transducer`] coalgebra
//! `Piece → (state, [Output])`, but they carry opposite delivery laws, and that is exactly the part
//! that must not be shared:
//!
//! - data ([`ReliableMessage`](crate::chunk::ReliableMessage)): completeness > timeliness — an
//!   order-free, idempotent fold that emits a message only once *every* chunk has arrived.
//! - media ([`MediaDepacketizer`]): timeliness > completeness — a time-ordered, **lossy** transducer
//!   that groups RTP packets into frames, emits them in timestamp order, and *drops* a frame it can
//!   no longer wait for rather than block the stream.
//!
//! ```text
//!   step : RtpPacket ↦ [MediaFrame]   -- group by timestamp, emit on marker, in order, drop late
//! ```
//!
//! Scope: this moves opaque RTP — no decode, no concealment, no congestion control. Reordering is
//! bounded by a frame-count window (clock-independent), and total memory by a pending-frame cap.
//! Intra-frame loss is detected by sequence-contiguity within a timestamp; sequence wrap inside a
//! single frame is assumed not to occur (frames are small).

use std::collections::BTreeMap;

use bytes::Bytes;
use rings_transport::core::media::RtpPacket;

use crate::chunk::ReassemblyStrategy;
use crate::chunk::Transducer;

/// One reassembled media frame: all packets sharing an RTP timestamp, concatenated in sequence
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    /// The RTP timestamp the frame was sampled at.
    pub timestamp: u32,
    /// The frame payload (depacketized, but still codec-opaque to us).
    pub data: Bytes,
}

/// Bounds the [`MediaDepacketizer`] enforces, as an explicit value (mirrors
/// [`ReassemblyLimits`](crate::chunk::ReassemblyLimits)): the shell supplies them, the strategy only
/// enforces what it is given, and tests use small values.
#[derive(Debug, Clone, Copy)]
pub struct MediaLimits {
    /// How many *newer* frames may pile up behind the oldest still-incomplete frame before we give
    /// up waiting for it and drop it (a clock-independent jitter window). Larger = more reordering
    /// tolerated, at the cost of more latency before a lost frame is abandoned.
    pub reorder_window: usize,
    /// Hard cap on buffered (in-flight) frames — bounds memory regardless of arrival pattern.
    pub max_pending_frames: usize,
    /// Largest a single frame's buffered data may grow; further packets for it are dropped.
    pub max_frame_bytes: usize,
}

impl MediaLimits {
    /// Production bounds. Clock-independent (frame counts), so they suit any payload type.
    pub fn production() -> Self {
        Self {
            reorder_window: 8,
            max_pending_frames: 64,
            max_frame_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self::production()
    }
}

/// One frame being assembled: its packets keyed by sequence number (dedup + order).
#[derive(Default)]
struct FrameBuf {
    /// sequence number → payload, ordered for in-sequence concat.
    packets: BTreeMap<u16, Bytes>,
    /// whether the frame's last packet (marker bit) has been seen.
    has_marker: bool,
    /// running sum of buffered payload bytes.
    data_bytes: usize,
}

impl FrameBuf {
    /// Complete iff the marker has arrived and the buffered sequence numbers are gap-free, i.e. the
    /// packets form a contiguous run `min..=max` (intra-frame loss ⇒ not complete).
    fn is_complete(&self) -> bool {
        if !self.has_marker || self.packets.is_empty() {
            return false;
        }
        let min = *self.packets.keys().next().unwrap();
        let max = *self.packets.keys().next_back().unwrap();
        (max - min) as usize + 1 == self.packets.len()
    }

    fn assemble(self) -> Bytes {
        self.packets.into_values().flatten().collect()
    }
}

/// Strategy: groups RTP packets into [`MediaFrame`]s and emits them in timestamp order, lossily.
///
/// Emission is in strictly increasing timestamp order. The oldest buffered frame gates the stream:
/// it is emitted once complete; if it stays incomplete while more than `reorder_window` newer frames
/// accumulate behind it, it is **dropped** (the missing packets are presumed lost) so the stream can
/// continue. Packets for a timestamp at or below the last emitted one are late and discarded.
pub struct MediaDepacketizer {
    /// in-flight frames keyed by RTP timestamp (ordered so "oldest" is `first`).
    frames: BTreeMap<u32, FrameBuf>,
    /// timestamp of the last frame emitted or dropped; anything `<=` it is late.
    watermark: Option<u32>,
    limits: MediaLimits,
}

/// Media depacketizer: the [`MediaDepacketizer`] strategy driven by the shared [`Transducer`].
pub type MediaReassembler = Transducer<MediaDepacketizer>;

impl Transducer<MediaDepacketizer> {
    /// Empty depacketizer with [`MediaLimits::production`] bounds.
    pub fn new() -> Self {
        Self::from_strategy(MediaDepacketizer::default())
    }

    /// Empty depacketizer enforcing the given `limits`.
    pub fn with_limits(limits: MediaLimits) -> Self {
        Self::from_strategy(MediaDepacketizer::with_limits(limits))
    }

    /// Number of frames currently buffered (incomplete or awaiting in-order emission).
    pub fn pending_count(&self) -> usize {
        self.strategy().frames.len()
    }
}

impl Default for MediaDepacketizer {
    fn default() -> Self {
        Self::with_limits(MediaLimits::production())
    }
}

impl MediaDepacketizer {
    /// Empty depacketizer enforcing the given `limits`.
    pub fn with_limits(limits: MediaLimits) -> Self {
        Self {
            frames: BTreeMap::new(),
            watermark: None,
            limits,
        }
    }

    /// Whether `ts` is in the past relative to what we have already emitted/dropped.
    fn is_late(&self, ts: u32) -> bool {
        self.watermark.is_some_and(|w| ts <= w)
    }

    /// Emit the in-order prefix of complete frames, dropping the oldest if it has stalled past the
    /// reorder window. Returns the frames to deliver, in timestamp order.
    fn drain_ready(&mut self) -> Vec<MediaFrame> {
        let mut out = Vec::new();
        while let Some((&ts, buf)) = self.frames.iter().next() {
            if buf.is_complete() {
                let buf = self.frames.remove(&ts).unwrap();
                self.watermark = Some(ts);
                out.push(MediaFrame {
                    timestamp: ts,
                    data: buf.assemble(),
                });
            } else if self.frames.len() > self.limits.reorder_window {
                // The oldest frame is still incomplete but newer frames have piled up past the
                // window: give up on it (lossy) so the stream can advance.
                self.frames.remove(&ts);
                self.watermark = Some(ts);
            } else {
                break;
            }
        }
        out
    }
}

impl ReassemblyStrategy for MediaDepacketizer {
    type Piece = RtpPacket;
    type Output = MediaFrame;

    fn step(&mut self, packet: RtpPacket) -> Vec<MediaFrame> {
        // Late packet for an already-passed frame: drop.
        if self.is_late(packet.timestamp) {
            return Vec::new();
        }

        // Make room for a brand-new frame: drop the oldest (lossy) rather than exceed the cap.
        if !self.frames.contains_key(&packet.timestamp)
            && self.frames.len() >= self.limits.max_pending_frames
        {
            if let Some((&oldest, _)) = self.frames.iter().next() {
                self.frames.remove(&oldest);
                self.watermark = Some(oldest);
            }
            // The just-dropped frame may have been newer than this packet; re-check lateness.
            if self.is_late(packet.timestamp) {
                return Vec::new();
            }
        }

        let buf = self.frames.entry(packet.timestamp).or_default();
        // Per-frame byte cap: ignore further payload once a frame is oversized.
        if buf.data_bytes + packet.payload.len() <= self.limits.max_frame_bytes {
            buf.has_marker |= packet.marker;
            buf.data_bytes += packet.payload.len();
            // First write per sequence wins (dedup retransmits).
            buf.packets.entry(packet.sequence).or_insert(packet.payload);
        }

        self.drain_ready()
    }

    fn gc(&mut self) {
        // Media has no wall-clock TTL here; the count-based reorder window in `step` already bounds
        // both latency and memory. `drain_ready` re-runs the same spill logic for callers that tick.
        let _ = self.drain_ready();
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use rings_transport::core::media::RtpPacket;

    use super::MediaFrame;
    use super::MediaLimits;
    use super::MediaReassembler;

    fn pkt(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            sequence: seq,
            timestamp: ts,
            marker,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    fn small() -> MediaReassembler {
        MediaReassembler::with_limits(MediaLimits {
            reorder_window: 2,
            max_pending_frames: 8,
            max_frame_bytes: 1024,
        })
    }

    #[test]
    fn reorders_packets_within_a_frame() {
        let mut r = small();
        assert!(r.handle(pkt(0, 100, false, b"a")).is_empty());
        assert!(r.handle(pkt(2, 100, true, b"c")).is_empty()); // marker but gap at seq 1
        let out = r.handle(pkt(1, 100, false, b"b")); // fills the gap -> complete
        assert_eq!(out, vec![MediaFrame {
            timestamp: 100,
            data: Bytes::from_static(b"abc")
        }]);
    }

    #[test]
    fn marker_completes_single_packet_frame() {
        let mut r = small();
        let out = r.handle(pkt(0, 100, true, b"x"));
        assert_eq!(out, vec![MediaFrame {
            timestamp: 100,
            data: Bytes::from_static(b"x")
        }]);
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn emits_in_timestamp_order() {
        let mut r = small();
        // ts=100 is buffered but incomplete (no marker yet).
        assert!(r.handle(pkt(0, 100, false, b"A")).is_empty());
        // ts=200 completes, but must wait behind the still-incomplete ts=100.
        assert!(r.handle(pkt(2, 200, true, b"B")).is_empty());
        // completing ts=100 flushes 100 then 200, in order.
        let out = r.handle(pkt(1, 100, true, b"a"));
        assert_eq!(out, vec![
            MediaFrame {
                timestamp: 100,
                data: Bytes::from_static(b"Aa")
            },
            MediaFrame {
                timestamp: 200,
                data: Bytes::from_static(b"B")
            },
        ]);
    }

    #[test]
    fn late_packet_is_dropped() {
        let mut r = small();
        assert_eq!(r.handle(pkt(0, 100, true, b"x")).len(), 1); // watermark = 100
        assert!(r.handle(pkt(9, 50, true, b"old")).is_empty()); // ts < watermark
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn stalled_oldest_frame_is_dropped_past_reorder_window() {
        let mut r = small(); // reorder_window = 2
                             // ts=100 never completes (gap at seq 1: only seq 0 with no marker).
        assert!(r.handle(pkt(0, 100, false, b"a")).is_empty());
        // pile up newer complete frames behind it.
        assert!(r.handle(pkt(10, 200, true, b"B")).is_empty());
        // third pending frame pushes len past the window -> drop ts=100, then flush 200 & 300.
        let out = r.handle(pkt(11, 300, true, b"C"));
        let times: Vec<u32> = out.iter().map(|f| f.timestamp).collect();
        assert_eq!(times, vec![200, 300]);
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn duplicate_sequence_is_ignored() {
        let mut r = small();
        assert!(r.handle(pkt(0, 100, false, b"a")).is_empty());
        assert!(r.handle(pkt(0, 100, false, b"DUP")).is_empty()); // same seq, ignored
        let out = r.handle(pkt(1, 100, true, b"b"));
        assert_eq!(out, vec![MediaFrame {
            timestamp: 100,
            data: Bytes::from_static(b"ab")
        }]);
    }

    #[test]
    fn pending_frames_are_capped() {
        let limits = MediaLimits {
            reorder_window: 1000, // don't spill via window; test the hard cap
            max_pending_frames: 4,
            max_frame_bytes: 1024,
        };
        let mut r = MediaReassembler::with_limits(limits);
        // each incomplete (no marker), distinct ts
        for ts in 0..20u32 {
            r.handle(pkt(0, ts * 10 + 1, false, b"x"));
        }
        assert!(r.pending_count() <= limits.max_pending_frames);
    }

    #[test]
    fn emitted_timestamps_are_monotonic() {
        let mut r = small();
        let mut last = 0u32;
        for ts in [300u32, 100, 200, 400] {
            for f in r.handle(pkt(0, ts, true, b"f")) {
                assert!(f.timestamp > last, "ts {} not after {}", f.timestamp, last);
                last = f.timestamp;
            }
        }
    }
}
