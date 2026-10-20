//! `Encipher` / `Decipher` (AEZ v5, §3): the tweakable wide-block permutation on
//! non-empty strings, dispatched by length to AEZ-tiny or AEZ-core.
//!
//! ```text
//! |X| = 0        ↦  X            (never enciphered; see `crate::Aez::encrypt`)
//! 0 < |X| < 32   ↦  AEZ-tiny_K^Δ(X)
//! |X| ≥ 32       ↦  AEZ-core_K^Δ(X)
//! ```
//!
//! Law: for every `Δ`, `Decipher_Δ ∘ Encipher_Δ = id` on each length class.

use crate::block::Block;
use crate::tbc::Subkeys;

/// Shortest string AEZ-core enciphers (`2 · 128` bits); shorter strings use AEZ-tiny.
const CORE_MIN_BYTES: usize = 32;

/// `d ∈ {0, 1}` of the AEZ pseudocode.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    /// `d = 0`: `Encipher`.
    Encipher,
    /// `d = 1`: `Decipher`.
    Decipher,
}

impl Direction {
    /// `1 + d`: the column used before the middle Feistel step of AEZ-core.
    pub(crate) const fn inner_column(self) -> u32 {
        match self {
            Self::Encipher => 1,
            Self::Decipher => 2,
        }
    }

    /// `2 − d`: the column used after the middle Feistel step of AEZ-core.
    pub(crate) const fn outer_column(self) -> u32 {
        match self {
            Self::Encipher => 2,
            Self::Decipher => 1,
        }
    }
}

/// `X ↦ Encipher_K^Δ(X)` or `Decipher_K^Δ(X)` in place, by `direction`.
pub(crate) fn apply(subkeys: &Subkeys, delta: Block, direction: Direction, text: &mut [u8]) {
    match text.len() {
        0 => {}
        1..CORE_MIN_BYTES => crate::aez_tiny::apply(subkeys, delta, direction, text),
        _ => crate::aez_core::apply(subkeys, delta, direction, text),
    }
}
