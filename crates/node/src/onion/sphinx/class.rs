//! Loop classes `b` and the carry width `C_b` (#834 D6, L3).
//!
//! A loop's class is one of the cell buckets (#834 D6: `16 KiB … 12 MiB`):
//!
//! ```text
//! 𝔅 = OnionCellBucket ≅ OnionLoopClass                      (From, and back by cell_bytes)
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

/// Bytes of one admission unit, 16 KiB: the least loop class (#834 L9).
pub(crate) const ONION_UNIT_BYTES: usize = 16 * 1024;

/// The class `b` of one loop, a cell bucket of `16 KiB … 12 MiB` (D6): every cell of the loop,
/// replies included, is exactly `b` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionLoopClass(OnionCellBucket);

impl OnionLoopClass {
    /// The default class, `b = 16 KiB`.
    pub(crate) const DEFAULT: Self = Self(OnionCellBucket::KiB16);

    /// `b`, the cell length.
    pub(crate) const fn cell_bytes(self) -> usize {
        self.0.cell_bytes()
    }

    /// `u(b) = b / 16 KiB`, the admission units one cell of the class costs its receiver, at
    /// least one since the least class is one unit (#834 L9).
    pub(crate) const fn units(self) -> u32 {
        (self.cell_bytes() / ONION_UNIT_BYTES) as u32
    }

    /// `C_b = b − |χ| − F`, the carry slot width.
    pub(crate) const fn carry_bytes(self) -> usize {
        self.cell_bytes() - ONION_HEADER_BYTES - ONION_CELL_FRAMING_BYTES
    }

    /// `C₀ = C_b − τ`, the width of a padded carry value.
    pub(crate) const fn carry_value_bytes(self) -> usize {
        self.carry_bytes() - ONION_CARRY_AUTHENTICATOR_BYTES
    }

    /// `C₀ − 1`, the widest value `pad` admits: `|v| < C₀` (L3).
    pub(crate) const fn value_capacity(self) -> usize {
        self.carry_value_bytes() - 1
    }

    /// `b` as the one-byte string the header MAC binds (#834 H1: `γ = MAC(b ‖ β)`): the
    /// bucket's `repr(u8)` discriminant, injective on classes.
    pub(crate) const fn mac_label(self) -> [u8; 1] {
        [self.0 as u8]
    }

    /// The class whose cells are `length` bytes long, if any: the decoder of `F = 0`.
    pub(crate) fn from_cell_bytes(length: usize) -> Option<Self> {
        OnionCellBucket::ALL
            .into_iter()
            .find(|bucket| bucket.cell_bytes() == length)
            .map(Self::from)
    }
}

impl From<OnionCellBucket> for OnionLoopClass {
    /// Every bucket is a loop class (D6).
    fn from(bucket: OnionCellBucket) -> Self {
        Self(bucket)
    }
}

// `DEFAULT` is the least class, `KiB16`; it leaves an
// AEZ-core carry slot (`C_b ≥ 32`) and room for the padding marker, and every larger class leaves
// more.
const _: () = assert!(
    OnionLoopClass::DEFAULT.cell_bytes() == OnionCellBucket::KiB16.cell_bytes()
        && OnionLoopClass::DEFAULT.carry_value_bytes() >= 32
);

// Every class is a whole number of units, and the largest (12 MiB = 768 units) fits `u32`, so
// `units` is exact.
const _: () = assert!(
    OnionLoopClass::DEFAULT.cell_bytes() == ONION_UNIT_BYTES
        && OnionCellBucket::MiB12
            .cell_bytes()
            .is_multiple_of(ONION_UNIT_BYTES)
        && OnionCellBucket::MiB12.cell_bytes() / ONION_UNIT_BYTES <= u32::MAX as usize
);
