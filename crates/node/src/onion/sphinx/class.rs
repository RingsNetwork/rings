//! Loop classes `b` and the carry width `C_b` (#834 D6, L3).
//!
//! A loop's class is one of today's cell buckets above 4 KiB, since the fixed header alone does
//! not leave a carry in 4 KiB:
//!
//! ```text
//! 𝔅 = OnionCellBucket ∖ {KiB4} ≅ OnionLoopClass            (TryFrom rejects KiB4)
//! C_b = b − |χ| − F          carry slot                     F = 0: a cell's class is its length
//! C₀  = C_b − τ              padded value width             τ = 16
//! ```
//!
//! Law (L5′): `C_b` is a function of `b` alone, never of the loop length `H` or a position `i`.
//! `C_b ≥ 32` for every class, which keeps every AEZ call on the AEZ-core path.

use super::carry::ONION_CARRY_AUTHENTICATOR_BYTES;
use super::header::ONION_HEADER_BYTES;
use crate::onion::circuit::OnionCellBucket;

/// Cell framing `F` beyond `χ ‖ y`: none, since a cell's class is its length.
pub(crate) const ONION_CELL_FRAMING_BYTES: usize = 0;

/// The class `b` of one loop, a cell bucket that leaves room for the header: every cell of the
/// loop, replies included, is exactly `b` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionLoopClass(OnionCellBucket);

/// The cell bucket below every loop class: `KiB4` cannot hold the fixed header.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("cell bucket {0:?} cannot hold the fixed onion header")]
pub(crate) struct OnionLoopClassError(OnionCellBucket);

impl OnionLoopClass {
    /// The default class, `b = 16 KiB`.
    pub(crate) const DEFAULT: Self = Self(OnionCellBucket::KiB16);

    /// `b`, the cell length.
    pub(crate) const fn cell_bytes(self) -> usize {
        self.0.plaintext_len()
    }

    /// `C_b = b − |χ| − F`, the carry slot width.
    pub(crate) const fn carry_bytes(self) -> usize {
        self.cell_bytes() - ONION_HEADER_BYTES - ONION_CELL_FRAMING_BYTES
    }

    /// `C₀ = C_b − τ`, the width of a padded carry value.
    pub(crate) const fn carry_value_bytes(self) -> usize {
        self.carry_bytes() - ONION_CARRY_AUTHENTICATOR_BYTES
    }

    /// `b` as the 8-byte big-endian string the header MAC binds (#834 H1: `γ = MAC(b ‖ β)`).
    ///
    /// `usize` is at most 64 bits wide on every target, so the widening cast is lossless.
    pub(crate) const fn mac_label(self) -> [u8; 8] {
        (self.cell_bytes() as u64).to_be_bytes()
    }

    /// The class whose cells are `length` bytes long, if any: the decoder of `F = 0`.
    pub(crate) fn from_cell_bytes(length: usize) -> Option<Self> {
        OnionCellBucket::ALL
            .into_iter()
            .find(|bucket| bucket.plaintext_len() == length)
            .and_then(|bucket| Self::try_from(bucket).ok())
    }
}

impl TryFrom<OnionCellBucket> for OnionLoopClass {
    type Error = OnionLoopClassError;

    /// Admit every bucket that exceeds the fixed header, that is every bucket but `KiB4`.
    fn try_from(bucket: OnionCellBucket) -> Result<Self, Self::Error> {
        match bucket {
            OnionCellBucket::KiB4 => Err(OnionLoopClassError(bucket)),
            _ => Ok(Self(bucket)),
        }
    }
}

// The least class leaves an AEZ-core carry slot (`C_b ≥ 32`) and room for the padding marker.
const _: () = assert!(OnionLoopClass::DEFAULT.carry_value_bytes() >= 32);
