//! Loop classes `b` and the carry width `C_b` (#834 D6, L3).
//!
//! A loop's class is one of today's cell buckets above 4 KiB, since the fixed header alone
//! exceeds 4 KiB. The cell on every edge is exactly `b` bytes, so
//!
//! ```text
//! C_b = b − |χ| − F          (carry slot)         F = 0: the class is the cell length
//! C₀  = C_b − τ              (padded value width)  τ = 16
//! ```
//!
//! Law (L5′): `C_b` is a function of `b` alone, never of the loop length `H` or a position `i`.
//! `C_b ≥ 32` for every class, which keeps every AEZ call on the AEZ-core path.

use super::carry::ONION_CARRY_AUTHENTICATOR_BYTES;
use super::header::ONION_HEADER_BYTES;
use crate::onion::circuit::OnionCellBucket;

/// Cell framing `F` beyond `χ ‖ y`: none, since a cell's class is its length.
pub const ONION_CELL_FRAMING_BYTES: usize = 0;

/// The class `b` of one loop: every cell of the loop, replies included, is `b` bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OnionLoopClass {
    /// `b = 16 KiB`, the default class.
    KiB16,
    /// `b = 64 KiB`.
    KiB64,
    /// `b = 256 KiB`.
    KiB256,
    /// `b = 1 MiB`.
    MiB1,
    /// `b = 4 MiB`.
    MiB4,
    /// `b = 12 MiB`.
    MiB12,
}

impl OnionLoopClass {
    /// Every class, in increasing `b`.
    pub const ALL: [Self; 6] = [
        Self::KiB16,
        Self::KiB64,
        Self::KiB256,
        Self::MiB1,
        Self::MiB4,
        Self::MiB12,
    ];

    /// `b`, the cell length: the size of the cell bucket this class is drawn from.
    pub const fn cell_bytes(self) -> usize {
        match self {
            Self::KiB16 => OnionCellBucket::KiB16,
            Self::KiB64 => OnionCellBucket::KiB64,
            Self::KiB256 => OnionCellBucket::KiB256,
            Self::MiB1 => OnionCellBucket::MiB1,
            Self::MiB4 => OnionCellBucket::MiB4,
            Self::MiB12 => OnionCellBucket::MiB12,
        }
        .plaintext_len()
    }

    /// `C_b = b − |χ| − F`, the carry slot width.
    pub const fn carry_bytes(self) -> usize {
        self.cell_bytes() - ONION_HEADER_BYTES - ONION_CELL_FRAMING_BYTES
    }

    /// `C₀ = C_b − τ`, the width of a padded carry value.
    pub const fn carry_value_bytes(self) -> usize {
        self.carry_bytes() - ONION_CARRY_AUTHENTICATOR_BYTES
    }

    /// The class whose cells are `length` bytes long, if any.
    pub fn from_cell_bytes(length: usize) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|class| class.cell_bytes() == length)
    }
}

// The least class leaves an AEZ-core carry slot (`C_b ≥ 32`) and room for the padding marker.
const _: () = assert!(OnionLoopClass::KiB16.carry_value_bytes() >= 32);
