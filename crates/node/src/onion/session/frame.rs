//! Session frames, the carried values of a world-facing session symbol (#834 D2′, D8).
//!
//! ```text
//! enc(data(n, φ, w))    = 00 ‖ n:u32 ‖ φ ‖ w       |w| ≤ C₀ − 7
//! enc(fin(n, φ))        = 01 ‖ n:u32 ‖ φ ‖ (|t|:u16 ‖ t  when T)
//! enc(credit(υ₁ … υ_k)) = 02 ‖ υ₁ ‖ … ‖ υ_k         |υ| = 2979, k ≥ 1
//! enc(abort(n))         = 03 ‖ n:u32
//! φ = T·2⁰,  T ⇒ w = |t|:u16 ‖ t ‖ w′              the other bits of φ are 0
//! ```
//!
//! `n` is the per-direction sequence of `data`, `fin` and `abort`, starting at 0; `credit` is
//! unsequenced and flows client-to-`h` only. `fin` ends a direction whose bytes really ended;
//! `abort` fails the session, carrying no reason (#834 D2′, #843 Q5), and is never an end of
//! stream: a receiver applies it on arrival, since nothing after it counts. A `fin` carries `T`
//! like `data` (#895 D-L5): a `fin` sent before the first reply names the target too, so it can
//! open the session at `h` even if it arrives before the session's first `data`. Its `n` is
//! informational: the sender's next sequence where it has one (`0` from an exit answering a
//! session it no longer holds), kept so every frame's contents are uniform; no receiver orders
//! an `abort` by it or reads it otherwise. Every frame fills
//! one uniform cell, so a relay cannot tell an `abort` from any other frame. The flag `T`
//! carries the session target `t` inline, so any loop can open the session and none is
//! dedicated to opening; its other bits are reserved for message-oriented symbols (#847) and
//! fail closed until then.
//!
//! Laws (tested in `session::tests`):
//!
//! - **Round trip.** `decode ∘ encode = Some` on every frame that fits its class, and `decode`
//!   is canonical: `decode(w) = Some(f) ⇒ encode(f) = w`.
//! - **Width** (L3). A frame whose encoding exceeds `C₀ − 1` bytes, the widest value `pad`
//!   admits, is refused by `encode`, never truncated: [`OnionFrame::data_capacity`] is exactly
//!   `C₀ − 7` for `data` without `T` (six bytes of frame, one of padding marker), and a `credit`
//!   frame holds at most `k = ⌊(C₀ − 2) / |υ|⌋` blocks (`k = 4` at 16 KiB).
//! - **Closure.** Every other tag byte, a reserved flag bit, a truncated field, a `credit` whose
//!   length is not a whole number of blocks, and trailing bytes after `fin`'s target or after
//!   `abort` are rejected.

use bytes::Bytes;
use zeroize::Zeroizing;

use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::cell::ONION_SURB_BYTES;
use crate::onion::sphinx::class::OnionLoopClass;

/// The tag byte of a `data` frame.
const DATA_TAG: u8 = 0x00;

/// The tag byte of a `fin` frame.
const FIN_TAG: u8 = 0x01;

/// The tag byte of a `credit` frame.
const CREDIT_TAG: u8 = 0x02;

/// The tag byte of an `abort` frame.
const ABORT_TAG: u8 = 0x03;

/// The flag `T` of `φ`: the data carries the session target inline.
const TARGET_FLAG: u8 = 0x01;

/// Bytes of a `data` frame before `w`: the tag, `n` and `φ`.
const DATA_OVERHEAD_BYTES: usize = 1 + 4 + 1;

/// Bytes of the target length `|t|` in front of `t`.
const TARGET_LENGTH_BYTES: usize = 2;

/// The per-direction sequence `n` of `data`, `fin` and `abort` frames (#834 D2′).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OnionSequence(u32);

impl OnionSequence {
    /// The first sequence of a direction, `n = 0`.
    pub(crate) const FIRST: Self = Self(0);

    /// Wrap a wire sequence.
    #[cfg(test)]
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The wire value.
    pub(crate) const fn value(self) -> u32 {
        self.0
    }

    /// `n + 1`, or `None` once the 32-bit sequence space is spent: a direction carries at most
    /// `2³²` frames, after which the session must close.
    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// One session frame; see the module documentation for its encoding.
#[derive(Debug)]
pub(crate) enum OnionFrame {
    /// `data(n, φ, w)`: world bytes, with the target inline while `T` is set.
    Data {
        /// `n`.
        sequence: OnionSequence,
        /// `t` when `T` is set: the session target's canonical authority.
        target: Option<Bytes>,
        /// `w′`: the stream bytes, possibly empty (a credit-only loop, or the open ack).
        payload: Bytes,
    },
    /// `fin(n, φ)`: this direction is closed after frame `n − 1`.
    Fin {
        /// `n`.
        sequence: OnionSequence,
        /// `t` when `T` is set: sent before the first reply, like `data`.
        target: Option<Bytes>,
    },
    /// `credit(υ₁ … υ_k)`: `k ≥ 1` further reply blocks for the session.
    Credit(Vec<OnionSurb>),
    /// `abort(n)`: the session failed at its sender; no reason is given, and `n` is
    /// informational (see the module documentation).
    Abort {
        /// `n`.
        sequence: OnionSequence,
    },
}

/// Why a frame was not encoded, so it must not be sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionFrameUnencodable {
    /// It does not fit the class (L3).
    #[error("session frame of {length} bytes exceeds the {capacity}-byte carry of its class")]
    TooWide {
        /// `|enc(frame)|`.
        length: usize,
        /// `C₀ − 1`, the widest carried value.
        capacity: usize,
    },
    /// A `credit` frame of no block, which no decoder accepts.
    #[error("credit frame of no reply block")]
    EmptyCredit,
}

/// Why a carried value is not a frame; the loop is dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionFrameError {
    /// The value is empty or its tag byte names no frame.
    #[error("unknown session frame tag")]
    Tag,
    /// A reserved bit of `φ` is set.
    #[error("reserved session flags {0:#04x}")]
    Flags(u8),
    /// A field ends early, `fin` has trailing bytes, or `credit` is not a whole number of
    /// blocks, or holds none.
    #[error("malformed session frame")]
    Malformed,
    /// A reply block of a `credit` frame does not decode.
    #[error("malformed reply block in a credit frame")]
    Surb,
}

impl OnionFrame {
    /// Whether the frame carries the session target `t` (`T` set): the frames that may open a
    /// session at `h`.
    pub(crate) const fn carries_target(&self) -> bool {
        matches!(
            self,
            Self::Data {
                target: Some(_),
                ..
            } | Self::Fin {
                target: Some(_),
                ..
            }
        )
    }

    /// Whether the frame is sequenced (`data`, `fin`, `abort`): a frame whose loss is a gap in
    /// its direction, unlike `credit`.
    pub(crate) const fn is_sequenced(&self) -> bool {
        !matches!(self, Self::Credit(_))
    }

    /// `C₀ − 7`, the most stream bytes one `data` frame without `T` carries in class `class`:
    /// the carried value is at most `C₀ − 1` bytes (the padding marker takes one), six of them
    /// the frame's tag, `n` and `φ`.
    pub(crate) const fn data_capacity(class: OnionLoopClass) -> usize {
        class.value_capacity() - DATA_OVERHEAD_BYTES
    }

    /// The most stream bytes one `data` frame carrying the target `target` holds in `class`,
    /// `C₀ − 9 − |t|`, or `None` if the target alone does not fit.
    pub(crate) fn data_capacity_with_target(class: OnionLoopClass, target: &[u8]) -> Option<usize> {
        Self::data_capacity(class).checked_sub(TARGET_LENGTH_BYTES + target.len())
    }

    /// `k = ⌊(C₀ − 2) / |υ|⌋`, the most reply blocks one `credit` frame holds in `class` (D8).
    pub(crate) const fn credit_capacity(class: OnionLoopClass) -> usize {
        (class.value_capacity() - 1) / ONION_SURB_BYTES
    }

    /// `enc(frame)`, refusing a frame wider than the carry of `class`, or a `credit` frame of no
    /// block. The encoding is zeroized on drop: a `credit` frame holds its blocks' seeds `σ_υ`.
    ///
    /// # Errors
    ///
    /// [`OnionFrameUnencodable`] if `|enc(frame)| > C₀ − 1`, a target longer than `u16::MAX`, or an
    /// empty `credit`.
    pub(crate) fn encode(
        &self,
        class: OnionLoopClass,
    ) -> Result<Zeroizing<Vec<u8>>, OnionFrameUnencodable> {
        let capacity = class.value_capacity();
        let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
        match self {
            Self::Data {
                sequence,
                target,
                payload,
            } => {
                bytes.push(DATA_TAG);
                bytes.extend_from_slice(&sequence.value().to_be_bytes());
                encode_target(&mut bytes, target.as_ref(), capacity)?;
                bytes.extend_from_slice(payload);
            }
            Self::Fin { sequence, target } => {
                bytes.push(FIN_TAG);
                bytes.extend_from_slice(&sequence.value().to_be_bytes());
                encode_target(&mut bytes, target.as_ref(), capacity)?;
            }
            Self::Abort { sequence } => {
                bytes.push(ABORT_TAG);
                bytes.extend_from_slice(&sequence.value().to_be_bytes());
            }
            Self::Credit(surbs) if surbs.is_empty() => {
                return Err(OnionFrameUnencodable::EmptyCredit);
            }
            Self::Credit(surbs) => {
                bytes.push(CREDIT_TAG);
                surbs.iter().for_each(|surb| surb.encode_into(&mut bytes));
            }
        }
        if bytes.len() > capacity {
            return Err(OnionFrameUnencodable::TooWide {
                length: bytes.len(),
                capacity,
            });
        }
        Ok(bytes)
    }

    /// `dec(w)` for a value carried in a class-`class` loop; the reply blocks of a `credit` take
    /// that class.
    ///
    /// # Errors
    ///
    /// The [`OnionFrameError`] of the first field that fails.
    pub(crate) fn decode(class: OnionLoopClass, bytes: &[u8]) -> Result<Self, OnionFrameError> {
        let (tag, rest) = bytes.split_first().ok_or(OnionFrameError::Tag)?;
        match *tag {
            DATA_TAG => {
                let (sequence, rest) = split_sequence(rest)?;
                let (target, payload) = split_target(rest)?;
                Ok(Self::Data {
                    sequence,
                    target,
                    payload: Bytes::copy_from_slice(payload),
                })
            }
            FIN_TAG => {
                let (sequence, rest) = split_sequence(rest)?;
                match split_target(rest)? {
                    (target, []) => Ok(Self::Fin { sequence, target }),
                    _ => Err(OnionFrameError::Malformed),
                }
            }
            ABORT_TAG => match split_sequence(rest)? {
                (sequence, []) => Ok(Self::Abort { sequence }),
                _ => Err(OnionFrameError::Malformed),
            },
            CREDIT_TAG => {
                let (blocks, []) = rest.as_chunks::<ONION_SURB_BYTES>() else {
                    return Err(OnionFrameError::Malformed);
                };
                if blocks.is_empty() {
                    return Err(OnionFrameError::Malformed);
                }
                blocks
                    .iter()
                    .map(|block| OnionSurb::decode(class, block).ok_or(OnionFrameError::Surb))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Self::Credit)
            }
            _ => Err(OnionFrameError::Tag),
        }
    }
}

/// Append `φ ‖ (|t| ‖ t when T)`: the flags byte and, for a `target`, the inline target.
///
/// # Errors
///
/// A target longer than `|t|:u16` can name, which no class carries.
fn encode_target(
    bytes: &mut Vec<u8>,
    target: Option<&Bytes>,
    capacity: usize,
) -> Result<(), OnionFrameUnencodable> {
    match target {
        Some(target) => {
            let length =
                u16::try_from(target.len()).map_err(|_| OnionFrameUnencodable::TooWide {
                    length: target.len(),
                    capacity,
                })?;
            bytes.push(TARGET_FLAG);
            bytes.extend_from_slice(&length.to_be_bytes());
            bytes.extend_from_slice(target);
        }
        None => bytes.push(0),
    }
    Ok(())
}

/// `φ ‖ (|t| ‖ t when T)` split off the front of a frame's rest: the target, and what follows.
///
/// # Errors
///
/// A truncated field, or a reserved flag bit.
fn split_target(bytes: &[u8]) -> Result<(Option<Bytes>, &[u8]), OnionFrameError> {
    let (flags, rest) = bytes.split_first().ok_or(OnionFrameError::Malformed)?;
    match *flags {
        0 => Ok((None, rest)),
        TARGET_FLAG => {
            let (length, rest) = rest
                .split_first_chunk::<TARGET_LENGTH_BYTES>()
                .ok_or(OnionFrameError::Malformed)?;
            let (target, rest) = rest
                .split_at_checked(usize::from(u16::from_be_bytes(*length)))
                .ok_or(OnionFrameError::Malformed)?;
            Ok((Some(Bytes::copy_from_slice(target)), rest))
        }
        reserved => Err(OnionFrameError::Flags(reserved)),
    }
}

/// `n` and the rest of a frame.
fn split_sequence(bytes: &[u8]) -> Result<(OnionSequence, &[u8]), OnionFrameError> {
    bytes
        .split_first_chunk::<4>()
        .map(|(sequence, rest)| (OnionSequence(u32::from_be_bytes(*sequence)), rest))
        .ok_or(OnionFrameError::Malformed)
}

// The four tags are pairwise distinct, so `decode` is a function of the tag byte.
const _: () = assert!(
    DATA_TAG != FIN_TAG
        && DATA_TAG != CREDIT_TAG
        && DATA_TAG != ABORT_TAG
        && FIN_TAG != CREDIT_TAG
        && FIN_TAG != ABORT_TAG
        && CREDIT_TAG != ABORT_TAG
);

// The D8 figure: four reply blocks fit a 16 KiB credit frame, and a data frame carries
// `C₀ − 7 = 13442` bytes there (`C₀ = 13449`).
const _: () = assert!(
    OnionFrame::credit_capacity(OnionLoopClass::DEFAULT) == 4
        && OnionFrame::data_capacity(OnionLoopClass::DEFAULT) == 13_442
);
