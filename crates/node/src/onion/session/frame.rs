//! Session frames, the carried values of a world-facing session symbol (#834 D2′, D8).
//!
//! ```text
//! enc(data(n, φ, w))    = 00 ‖ n:u32 ‖ φ ‖ w       |w| ≤ C₀ − 7
//! enc(fin(n))           = 01 ‖ n:u32
//! enc(credit(υ₁ … υ_k)) = 02 ‖ υ₁ ‖ … ‖ υ_k         |υ| = 2979, k ≥ 1
//! φ = T·2⁰,  T ⇒ w = |t|:u16 ‖ t ‖ w′              the other bits of φ are 0
//! ```
//!
//! `n` is the per-direction sequence of `data` and `fin`, starting at 0; `credit` is unsequenced
//! and flows client-to-`h` only. The flag `T` carries the session target `t` inline, so any loop
//! can open the session and none is dedicated to opening; its other bits are reserved for
//! message-oriented symbols (#847) and fail closed until then.
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
//!   length is not a whole number of blocks, and trailing bytes after `fin` are rejected.

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

/// The flag `T` of `φ`: the data carries the session target inline.
const TARGET_FLAG: u8 = 0x01;

/// Bytes of a `data` frame before `w`: the tag, `n` and `φ`.
const DATA_OVERHEAD_BYTES: usize = 1 + 4 + 1;

/// Bytes of the target length `|t|` in front of `t`.
const TARGET_LENGTH_BYTES: usize = 2;

/// The per-direction sequence `n` of `data` and `fin` frames (#834 D2′).
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
    /// `fin(n)`: this direction is closed after frame `n − 1`.
    Fin {
        /// `n`.
        sequence: OnionSequence,
    },
    /// `credit(υ₁ … υ_k)`: `k ≥ 1` further reply blocks for the session.
    Credit(Vec<OnionSurb>),
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
                match target {
                    Some(target) => {
                        let length = u16::try_from(target.len()).map_err(|_| {
                            OnionFrameUnencodable::TooWide {
                                length: target.len(),
                                capacity,
                            }
                        })?;
                        bytes.push(TARGET_FLAG);
                        bytes.extend_from_slice(&length.to_be_bytes());
                        bytes.extend_from_slice(target);
                    }
                    None => bytes.push(0),
                }
                bytes.extend_from_slice(payload);
            }
            Self::Fin { sequence } => {
                bytes.push(FIN_TAG);
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
                let (flags, rest) = rest.split_first().ok_or(OnionFrameError::Malformed)?;
                let (target, payload) = match *flags {
                    0 => (None, rest),
                    TARGET_FLAG => {
                        let (length, rest) = rest
                            .split_first_chunk::<TARGET_LENGTH_BYTES>()
                            .ok_or(OnionFrameError::Malformed)?;
                        let (target, payload) = rest
                            .split_at_checked(usize::from(u16::from_be_bytes(*length)))
                            .ok_or(OnionFrameError::Malformed)?;
                        (Some(Bytes::copy_from_slice(target)), payload)
                    }
                    reserved => return Err(OnionFrameError::Flags(reserved)),
                };
                Ok(Self::Data {
                    sequence,
                    target,
                    payload: Bytes::copy_from_slice(payload),
                })
            }
            FIN_TAG => match split_sequence(rest)? {
                (sequence, []) => Ok(Self::Fin { sequence }),
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

/// `n` and the rest of a frame.
fn split_sequence(bytes: &[u8]) -> Result<(OnionSequence, &[u8]), OnionFrameError> {
    bytes
        .split_first_chunk::<4>()
        .map(|(sequence, rest)| (OnionSequence(u32::from_be_bytes(*sequence)), rest))
        .ok_or(OnionFrameError::Malformed)
}

// The D8 figure: four reply blocks fit a 16 KiB credit frame, and a data frame carries
// `C₀ − 7 = 13442` bytes there (`C₀ = 13449`).
const _: () = assert!(
    OnionFrame::credit_capacity(OnionLoopClass::DEFAULT) == 4
        && OnionFrame::data_capacity(OnionLoopClass::DEFAULT) == 13_442
);
