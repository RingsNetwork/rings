//! Loop classes `b` and the carry width `C_b` (#834 D6, L3).
//!
//! A loop's class is one of six cell lengths (#834 D6: `16 KiB … 12 MiB`), and a cell of the loop
//! is exactly `b` bytes: the class is the cell's length, visible to every hop, and the only size a
//! hop can observe.
//!
//! ```text
//! 𝔅   = {16 KiB, 64 KiB, 256 KiB, 1 MiB, 4 MiB, 12 MiB}   OnionLoopClass
//! u(b) = b / 16 KiB         admission units              u(16 KiB) = 1, u(12 MiB) = 768
//! C_b = b − |χ| − F          carry slot                     F = 0: a cell's class is its length
//! C₀  = C_b − τ              padded value width             τ = 16
//! ```
//!
//! Law (L5′): `C_b` is a function of `b` alone, never of the loop length `H` or a position `i`.
//! `C_b ≥ 32` for every class, which keeps every AEZ call on the AEZ-core path.

use std::num::NonZeroU32;

use super::carry::ONION_CARRY_AUTHENTICATOR_BYTES;
use super::header::ONION_HEADER_BYTES;

/// Cell framing `F` beyond `χ ‖ y`: none, since a cell's class is its length.
pub(crate) const ONION_CELL_FRAMING_BYTES: usize = 0;

/// The class `b` of one loop, `16 KiB … 12 MiB` (D6): every cell of the loop, replies included,
/// is exactly `b` bytes.
///
/// The class is intentionally visible while everything inside the cell is not. A small class set
/// bounds padding overhead without exposing a byte-accurate traffic fingerprint. The
/// discriminant is the one-byte label the header MAC binds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum OnionLoopClass {
    /// Sixteen KiB, the default and least class: one admission unit.
    KiB16,
    /// Sixty-four KiB.
    KiB64,
    /// 256 KiB.
    KiB256,
    /// One MiB.
    MiB1,
    /// Four MiB.
    MiB4,
    /// Twelve MiB, the largest class: `u_max = 768` units.
    MiB12,
}

impl OnionLoopClass {
    /// The default class, `b = 16 KiB`.
    pub(crate) const DEFAULT: Self = Self::KiB16;

    /// Every class, in increasing size.
    pub(crate) const ALL: [Self; 6] = [
        Self::KiB16,
        Self::KiB64,
        Self::KiB256,
        Self::MiB1,
        Self::MiB4,
        Self::MiB12,
    ];

    /// `b`, the cell length.
    pub(crate) const fn cell_bytes(self) -> usize {
        match self {
            Self::KiB16 => 16 * 1024,
            Self::KiB64 => 64 * 1024,
            Self::KiB256 => 256 * 1024,
            Self::MiB1 => 1024 * 1024,
            Self::MiB4 => 4 * 1024 * 1024,
            Self::MiB12 => 12 * 1024 * 1024,
        }
    }

    /// `u(b) = b / 16 KiB`, the admission units one cell of the class costs its receiver (#834
    /// L9): `1 + (u(b) − 1)`, exact since every class is a positive whole number of units that
    /// fits `u32` (asserted below).
    pub(crate) const fn units(self) -> NonZeroU32 {
        let units = self.cell_bytes() / Self::DEFAULT.cell_bytes();
        NonZeroU32::MIN.saturating_add(units.saturating_sub(1) as u32)
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
    /// `repr(u8)` discriminant, injective on classes.
    pub(crate) const fn mac_label(self) -> [u8; 1] {
        [self as u8]
    }

    /// The class whose cells are `length` bytes long, if any: the decoder of `F = 0`.
    pub(crate) fn from_cell_bytes(length: usize) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|class| class.cell_bytes() == length)
    }
}

// `DEFAULT` is the least class, `KiB16`; it leaves an
// AEZ-core carry slot (`C_b ≥ 32`) and room for the padding marker, and every larger class leaves
// more.
const _: () = assert!(OnionLoopClass::DEFAULT.carry_value_bytes() >= 32);

// Every class is a whole number of units, and the largest (12 MiB = 768 units) fits `u32`, so
// `units` is exact.
const _: () = assert!(
    OnionLoopClass::MiB12
        .cell_bytes()
        .is_multiple_of(OnionLoopClass::DEFAULT.cell_bytes())
        && OnionLoopClass::MiB12.cell_bytes() / OnionLoopClass::DEFAULT.cell_bytes()
            <= u32::MAX as usize
        && OnionLoopClass::MiB12.units().get() == 768
);
