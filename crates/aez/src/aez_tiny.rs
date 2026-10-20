//! AEZ-tiny (AEZ v5, §3, Fig. 3): the enciphering scheme for strings of 1 to 31 bytes.
//!
//! A string of `|M|` bytes is `m = 8|M|` bits, i.e. `2|M|` nibbles. AEZ-tiny is a balanced
//! Feistel network on the halves `L = M[1..n]`, `R = M[n+1..m]` of `n = m/2 = 4|M|` bits
//! each, so a half is exactly `|M|` nibbles, the unit this module splits and joins on:
//!
//! ```text
//! round_i : (L, R) ↦ (R, L ⊕ E^{0,j}(Δ ⊕ R10* ⊕ [i]_128)[1..n])
//!
//! Encipher:  (L, R) ──round_0 … round_{k−1}──▶ (L', R') ↦ C = R' ‖ L' ──[m < 128: φ]──▶ C
//! Decipher:  C ──[m < 128: φ]──▶ (L, R) ──round_{k−1} … round_0──▶ (L', R') ↦ M = R' ‖ L'
//!
//! φ(C) = C ⊕ (E^{0,3}(Δ ⊕ (C ∨ 10*)) ∧ 10*)        (rewrites only the first bit of C)
//! ```
//!
//! with `j = 6` if `m ≥ 128`, else `j = 7`, and `k` rounds from [`round_count`]. `φ` is an
//! involution (its mask does not read the bit it rewrites), and a Feistel network run with
//! its round indices reversed is its own inverse after the half swap, hence
//! `Decipher ∘ Encipher = id`.

use crate::block::Block;
use crate::block::BLOCK_BYTES;
use crate::cipher::Direction;
use crate::tbc::Subkeys;

/// Column of `E^{0,·}` used by the first-bit correction `φ`.
const FIRST_BIT_COLUMN: u32 = 3;

/// Round-function column for strings of at least one block.
const BLOCK_COLUMN: u32 = 6;

/// Round-function column for strings shorter than one block.
const SUB_BLOCK_COLUMN: u32 = 7;

/// `k`: 24 rounds for one byte, 16 for two, 10 below a block, 8 from a block on.
fn round_count(length: usize) -> u128 {
    match length {
        1 => 24,
        2 => 16,
        3..BLOCK_BYTES => 10,
        _ => 8,
    }
}

/// AEZ-tiny in place: `M ↦ C` for `d = direction`.
///
/// Pre: `1 ≤ |M| < 32` (so each half fits a block).
pub(crate) fn apply(subkeys: &Subkeys, delta: Block, direction: Direction, text: &mut [u8]) {
    let sub_block = text.len() < BLOCK_BYTES;
    let feistel = Feistel {
        subkeys,
        delta,
        half_nibbles: text.len(),
        column: if sub_block {
            SUB_BLOCK_COLUMN
        } else {
            BLOCK_COLUMN
        },
    };
    let rounds = 0..round_count(text.len());
    match direction {
        Direction::Encipher => {
            feistel.run(text, rounds);
            if sub_block {
                correct_first_bit(subkeys, delta, text);
            }
        }
        Direction::Decipher => {
            if sub_block {
                correct_first_bit(subkeys, delta, text);
            }
            feistel.run(text, rounds.rev());
        }
    }
}

/// The Feistel network of one AEZ-tiny call.
struct Feistel<'a> {
    /// The key.
    subkeys: &'a Subkeys,
    /// The tweak digest `Δ`.
    delta: Block,
    /// `|M|`: nibbles per half, `n = 4|M|` bits.
    half_nibbles: usize,
    /// `j` of the round function.
    column: u32,
}

impl Feistel<'_> {
    /// `(L, R) ↦ round_{i_last} ∘ … ∘ round_{i_first} (L, R)`, then writes `R' ‖ L'`.
    fn run(&self, text: &mut [u8], indices: impl Iterator<Item = u128>) {
        let halves = (
            take_nibbles(text, 0, self.half_nibbles),
            take_nibbles(text, self.half_nibbles, self.half_nibbles),
        );
        let (left, right) = indices.fold(halves, |(left, right), index| {
            (right, left ^ self.round_function(right, index))
        });
        put_nibbles(text, 0, self.half_nibbles, right);
        put_nibbles(text, self.half_nibbles, self.half_nibbles, left);
    }

    /// `E^{0,j}(Δ ⊕ R10* ⊕ [i]_128)[1..n]`.
    fn round_function(&self, right: Block, index: u128) -> Block {
        let bits = self.half_nibbles.saturating_mul(4);
        self.subkeys
            .e(
                0,
                self.column,
                self.delta ^ right.pad_bits(bits) ^ Block::from_index(index),
            )
            .truncate(bits)
    }
}

/// `φ(C) = C ⊕ (E^{0,3}(Δ ⊕ (C ∨ 10*)) ∧ 10*)`, in place. Pre: `|C| < 16`.
fn correct_first_bit(subkeys: &Subkeys, delta: Block, text: &mut [u8]) {
    let marked = Block::from_prefix(text) | Block::TOP_BIT;
    (subkeys.e(0, FIRST_BIT_COLUMN, delta ^ marked) & Block::TOP_BIT).xor_into(text);
}

/// Nibble `index` (0 = high nibble of byte 0) of `bytes`; `0` past the end.
fn nibble(bytes: &[u8], index: usize) -> u8 {
    bytes
        .get(index / 2)
        .map_or(0, |byte| (byte >> nibble_shift(index)) & 0x0f)
}

/// Replaces nibble `index` of `bytes` by the low nibble of `value`; no-op past the end.
fn set_nibble(bytes: &mut [u8], index: usize, value: u8) {
    if let Some(byte) = bytes.get_mut(index / 2) {
        let shift = nibble_shift(index);
        *byte = (*byte & !(0x0f << shift)) | ((value & 0x0f) << shift);
    }
}

/// Bit offset of nibble `index` inside its byte: 4 for the high nibble, 0 for the low.
fn nibble_shift(index: usize) -> u32 {
    if index.is_multiple_of(2) {
        4
    } else {
        0
    }
}

/// Nibbles `[start, start + count)` of `source` as a left-aligned block, zero-extended.
/// Pre: `count ≤ 32`.
fn take_nibbles(source: &[u8], start: usize, count: usize) -> Block {
    let mut bytes = [0; BLOCK_BYTES];
    (0..count).for_each(|index| {
        set_nibble(
            &mut bytes,
            index,
            nibble(source, start.saturating_add(index)),
        )
    });
    Block::from_bytes(bytes)
}

/// Writes the first `count` nibbles of `block` into `target` from nibble `start`.
fn put_nibbles(target: &mut [u8], start: usize, count: usize, block: Block) {
    let bytes = block.to_bytes();
    (0..count)
        .for_each(|index| set_nibble(target, start.saturating_add(index), nibble(&bytes, index)));
}
