//! The carrier of AEZ: 128-bit blocks as elements of `GF(2^128)`.
//!
//! A block is a string `X ∈ {0,1}^128`, read big-endian: the first bit of the first byte is
//! the coefficient of `x^127`. Under that reading `(Block, ⊕)` is the additive group of
//! `F = GF(2)[x]/(x^128 + x^7 + x^2 + x + 1)` and AEZ uses exactly two multiplicative
//! operations of `F` (AEZ v5, §3):
//!
//! - doubling `X ↦ 2·X`, a left shift reduced by `0x87` when the leading bit falls off;
//! - the scalar action `k·X` of an integer `k`, read as the polynomial whose coefficients
//!   are the bits of `k`; it is the unique `ℤ`-linear extension of doubling with
//!   `0·X = 0`, `1·X = X`, `(2k)·X = 2·(k·X)` and `(2k+1)·X = (2k)·X ⊕ X`.
//!
//! Both are `F₂`-linear: `2·(X ⊕ Y) = 2·X ⊕ 2·Y` and `k·(X ⊕ Y) = k·X ⊕ k·Y`.
//!
//! Every operation here is branch-free in the block's value: control flow depends only
//! on public integers (`k`, bit counts), never on key-derived bits.

use core::ops::BitAnd;
use core::ops::BitOr;
use core::ops::BitXor;

use zeroize::Zeroize;

/// Block length in bytes (`n = 128` bits).
pub(crate) const BLOCK_BYTES: usize = 16;

/// Block length in bits.
pub(crate) const BLOCK_BITS: usize = 128;

/// The reduction constant of `x^128 = x^7 + x^2 + x + 1` in `F`.
const REDUCTION: u128 = 0x87;

/// An element of `GF(2^128)` in AEZ's big-endian bit order.
///
/// `Copy` is sound for this carrier: a block is an identity-free value of the field, and
/// duplicating it changes no state. Blocks that hold key material live inside
/// [`crate::tbc::Subkeys`], which zeroizes them on drop.
#[derive(Clone, Copy, Default, Zeroize)]
pub(crate) struct Block(u128);

impl Block {
    /// The additive identity `0^128`.
    pub(crate) const ZERO: Self = Self(0);

    /// The string `10^127`: the padding `ε10*` of the empty string. Its leading bit is also
    /// the mask that AEZ-tiny uses for the first bit of a short ciphertext.
    pub(crate) const TOP_BIT: Self = Self(1 << (BLOCK_BITS - 1));

    /// Reads a block from its 16-byte encoding.
    pub(crate) const fn from_bytes(bytes: [u8; BLOCK_BYTES]) -> Self {
        Self(u128::from_be_bytes(bytes))
    }

    /// The 16-byte encoding; inverse of [`Block::from_bytes`].
    pub(crate) const fn to_bytes(self) -> [u8; BLOCK_BYTES] {
        self.0.to_be_bytes()
    }

    /// `[k]_128`: the integer `k` encoded as a 128-bit big-endian string.
    pub(crate) const fn from_index(index: u128) -> Self {
        Self(index)
    }

    /// `X ↦ X ‖ 0^*` truncated to 128 bits: the first `min(16, |prefix|)` bytes of `prefix`,
    /// zero-extended.
    pub(crate) fn from_prefix(prefix: &[u8]) -> Self {
        let mut bytes = [0; BLOCK_BYTES];
        bytes
            .iter_mut()
            .zip(prefix)
            .for_each(|(target, source)| *target = *source);
        Self::from_bytes(bytes)
    }

    /// `X ↦ X10*`: pads a partial block of `|X| < 16` bytes with one `1` bit and zeros.
    ///
    /// Pre: `partial.len() < 16`. A full block has no padded form in AEZ; given one, the
    /// result is the block itself, so the function stays total.
    pub(crate) fn padded(partial: &[u8]) -> Self {
        Self::from_prefix(partial) | Self::bit_at(partial.len().saturating_mul(8))
    }

    /// XORs the first `min(16, |target|)` bytes of the encoding into `target`.
    pub(crate) fn xor_into(self, target: &mut [u8]) {
        target
            .iter_mut()
            .zip(self.to_bytes())
            .for_each(|(target, source)| *target ^= source);
    }

    /// `2·X` in `F`.
    ///
    /// Branch-free: the reduction is masked by the broadcast of the leading bit instead of
    /// being selected by it, so the running time is independent of `X`.
    pub(crate) const fn double(self) -> Self {
        let carry = (self.0 >> (BLOCK_BITS - 1)).wrapping_neg();
        Self((self.0 << 1) ^ (carry & REDUCTION))
    }

    /// `k·X` in `F`, by double-and-add over the bits of `k` from the most significant.
    ///
    /// Law: `k·X = ⊕_{b : bit b of k is set} 2^b·X`. The loop runs over the bit length of the
    /// public `k`; each step adds `X` masked by bit `b`, so no branch reads `X`.
    pub(crate) fn times(self, scalar: u128) -> Self {
        let width = u128::BITS - scalar.leading_zeros();
        (0..width).rev().fold(Self::ZERO, |sum, bit| {
            let selected = ((scalar >> bit) & 1).wrapping_neg();
            Self(sum.double().0 ^ (self.0 & selected))
        })
    }

    /// `2^e·X` in `F`: `e` doublings.
    pub(crate) fn times_power_of_two(self, exponent: u32) -> Self {
        (0..exponent).fold(self, |block, _| block.double())
    }

    /// The first `bits` bits of `X`, followed by zeros.
    ///
    /// Pre: `bits ≤ 128`; larger values keep the whole block.
    pub(crate) fn truncate(self, bits: usize) -> Self {
        self & Self::leading_ones(bits)
    }

    /// The first `bits` bits of `X` followed by `10*`: the AEZ padding of a bit string that
    /// is not byte-aligned. Pre: `bits < 128` and `X` has zeros after bit `bits`.
    pub(crate) fn pad_bits(self, bits: usize) -> Self {
        self | Self::bit_at(bits)
    }

    /// The block whose only set bit is at position `position` (0 is the leading bit);
    /// zero when `position ≥ 128`.
    fn bit_at(position: usize) -> Self {
        u32::try_from(position)
            .ok()
            .and_then(|shift| Self::TOP_BIT.0.checked_shr(shift))
            .map_or(Self::ZERO, Self)
    }

    /// The block `1^bits 0^(128 − bits)`; all ones when `bits ≥ 128`.
    fn leading_ones(bits: usize) -> Self {
        u32::try_from(BLOCK_BITS.saturating_sub(bits))
            .ok()
            .and_then(|zeros| u128::MAX.checked_shl(zeros))
            .map_or(Self::ZERO, Self)
    }

    /// Whether `X = 0^128`, decided in constant time.
    pub(crate) fn is_zero(self) -> bool {
        subtle::ConstantTimeEq::ct_eq(&self.0, &0).into()
    }
}

impl BitXor for Block {
    type Output = Self;

    /// Addition in `F`.
    fn bitxor(self, other: Self) -> Self {
        Self(self.0 ^ other.0)
    }
}

impl BitAnd for Block {
    type Output = Self;

    /// Bitwise conjunction of the two strings (a mask, not a field operation).
    fn bitand(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl BitOr for Block {
    type Output = Self;

    /// Bitwise disjunction of the two strings (padding, not a field operation).
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}
