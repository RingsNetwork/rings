//! AEZ-core (AEZ v5, §3, Fig. 3): the enciphering scheme for strings of at least 32 bytes.
//!
//! A string of `|M| ≥ 32` bytes is laid out as
//!
//! ```text
//! M = M_1 M'_1 … M_ℓ M'_ℓ · M_uv · M_x M_y       |M_i| = |M'_i| = |M_x| = |M_y| = 16,
//!                                                |M_uv| = (|M| − 32) mod 32 < 32,
//! M_uv = M_u M_v                                 |M_u| = min(16, |M_uv|)
//! ```
//!
//! and processed in place in two passes around a middle Feistel step:
//!
//! ```text
//!          ┌───────────── pass 1 (per pair i) ─────────────┐
//! M_i,M'_i │ W_i = M_i ⊕ E^{1,i}(M'_i)                     │  X = ⊕ X_i ⊕ ρ(M_uv)
//!          │ X_i = M'_i ⊕ E^{0,0}(W_i)                     │
//!          └───────────────────────────────────────────────┘
//!                              │
//!                              ▼
//!          S_x = M_x ⊕ Δ ⊕ X ⊕ E^{0,1+d}(M_y)
//!          S_y = M_y ⊕ E^{−1,1+d}(S_x)              S = S_x ⊕ S_y
//!                              │
//!                              ▼
//!          ┌───────────── pass 2 (per pair i) ─────────────┐
//! W_i,X_i  │ S'_i = E^{2,i}(S)                             │  C_uv = M_uv ⊕ κ(S)
//!          │ Y_i = W_i ⊕ S'_i,  Z_i = X_i ⊕ S'_i           │  Y = ⊕ Y_i ⊕ ρ(C_uv)
//!          │ C'_i = Y_i ⊕ E^{0,0}(Z_i)                     │
//!          │ C_i  = Z_i ⊕ E^{1,i}(C'_i)                    │
//!          └───────────────────────────────────────────────┘
//!                              │
//!                              ▼
//!          C_y = S_x ⊕ E^{−1,2−d}(S_y)
//!          C_x = S_y ⊕ Δ ⊕ Y ⊕ E^{0,2−d}(C_y)
//! ```
//!
//! where `d = 0` enciphers and `d = 1` deciphers, `ρ` is the residue digest
//! ([`Residue::digest`]) and `κ` the residue mask ([`Residue::mask`]). The output is
//! `C_1 C'_1 … C_ℓ C'_ℓ C_uv C_x C_y`, written over the input.

use crate::block::Block;
use crate::block::BLOCK_BYTES;
use crate::cipher::Direction;
use crate::tbc::Subkeys;

/// Row of `E` that masks the first half of each pair (`E^{1,i}`).
const PAIR_ROW: u128 = 1;

/// Row of `E` that expands `S` over the pairs in pass 2 (`E^{2,i}`).
const SPREAD_ROW: u128 = 2;

/// Column of `E^{0,·}` and `E^{−1,·}` that digests and masks `M_u`.
const RESIDUE_U_COLUMN: u32 = 4;

/// Column of `E^{0,·}` and `E^{−1,·}` that digests and masks `M_v`.
const RESIDUE_V_COLUMN: u32 = 5;

/// The in-place view of a string of at least 32 bytes.
struct Layout<'a> {
    /// `(M_i, M'_i)` for `i = 1..ℓ`.
    pairs: &'a mut [[[u8; BLOCK_BYTES]; 2]],
    /// `M_uv`, fewer than 32 bytes.
    residue: &'a mut [u8],
    /// `M_x`.
    x: &'a mut [u8; BLOCK_BYTES],
    /// `M_y`.
    y: &'a mut [u8; BLOCK_BYTES],
}

impl<'a> Layout<'a> {
    /// Splits `M` as in the module diagram; `None` iff `|M| < 32`.
    fn split(message: &'a mut [u8]) -> Option<Self> {
        let (rest, y) = message.split_last_chunk_mut::<BLOCK_BYTES>()?;
        let (rest, x) = rest.split_last_chunk_mut::<BLOCK_BYTES>()?;
        let aligned = rest.len() - rest.len() % (2 * BLOCK_BYTES);
        let (pairs, residue) = rest.split_at_mut_checked(aligned)?;
        // `aligned` is a multiple of 32, so both chunkings leave empty remainders.
        let (pairs, _) = pairs.as_chunks_mut::<BLOCK_BYTES>().0.as_chunks_mut::<2>();
        Some(Self {
            pairs,
            residue,
            x,
            y,
        })
    }
}

/// `M_uv` split as `M_u M_v`, by the three cases of AEZ-core.
enum Residue<'a> {
    /// `|M_uv| = 0`.
    Empty,
    /// `0 < |M_uv| < 16`: only `M_u`, shorter than a block.
    Short(&'a mut [u8]),
    /// `16 ≤ |M_uv| < 32`: a full `M_u` and `|M_v| < 16`.
    Long(&'a mut [u8; BLOCK_BYTES], &'a mut [u8]),
}

impl<'a> Residue<'a> {
    /// Classifies `M_uv`.
    fn of(residue: &'a mut [u8]) -> Self {
        // |M_uv| < 32, so there is at most one full block and `[u, ..]` binds `M_u`.
        match residue.as_chunks_mut::<BLOCK_BYTES>() {
            ([], []) => Self::Empty,
            ([], u) => Self::Short(u),
            ([u, ..], v) => Self::Long(u, v),
        }
    }

    /// `ρ(M_uv)`: `0`, `E^{0,4}(M_u 10*)`, or `E^{0,4}(M_u) ⊕ E^{0,5}(M_v 10*)`.
    fn digest(&self, subkeys: &Subkeys) -> Block {
        match self {
            Self::Empty => Block::ZERO,
            Self::Short(u) => subkeys.e(0, RESIDUE_U_COLUMN, Block::padded(u)),
            Self::Long(u, v) => {
                subkeys.e(0, RESIDUE_U_COLUMN, Block::from_bytes(**u))
                    ^ subkeys.e(0, RESIDUE_V_COLUMN, Block::padded(v))
            }
        }
    }

    /// `M_uv ↦ M_uv ⊕ κ(S)`, with `κ(S) = E^{−1,4}(S) ‖ E^{−1,5}(S)` truncated to `|M_uv|`.
    /// An involution for fixed `S`.
    fn mask(&mut self, subkeys: &Subkeys, s: Block) {
        match self {
            Self::Empty => {}
            Self::Short(u) => subkeys.e_aes10(u128::from(RESIDUE_U_COLUMN), s).xor_into(u),
            Self::Long(u, v) => {
                subkeys
                    .e_aes10(u128::from(RESIDUE_U_COLUMN), s)
                    .xor_into(u.as_mut_slice());
                subkeys.e_aes10(u128::from(RESIDUE_V_COLUMN), s).xor_into(v);
            }
        }
    }
}

/// AEZ-core in place: `M ↦ C` for `d = direction`.
///
/// Pre: `|M| ≥ 32`; shorter strings belong to AEZ-tiny and are left unchanged.
pub(crate) fn apply(subkeys: &Subkeys, delta: Block, direction: Direction, message: &mut [u8]) {
    let Some(Layout {
        pairs,
        residue,
        x,
        y,
    }) = Layout::split(message)
    else {
        return;
    };
    let mut residue = Residue::of(residue);
    let (m_x, m_y) = (Block::from_bytes(*x), Block::from_bytes(*y));

    let sum_x = first_pass(subkeys, pairs) ^ residue.digest(subkeys);
    let s_x = m_x ^ delta ^ sum_x ^ subkeys.e(0, direction.inner_column(), m_y);
    let s_y = m_y ^ subkeys.e_aes10(u128::from(direction.inner_column()), s_x);
    let s = s_x ^ s_y;

    residue.mask(subkeys, s);
    let sum_y = second_pass(subkeys, pairs, s) ^ residue.digest(subkeys);
    let c_y = s_x ^ subkeys.e_aes10(u128::from(direction.outer_column()), s_y);
    let c_x = s_y ^ delta ^ sum_y ^ subkeys.e(0, direction.outer_column(), c_y);

    *x = c_x.to_bytes();
    *y = c_y.to_bytes();
}

/// Pass 1: `(M_i, M'_i) ↦ (W_i, X_i)` in place; returns `⊕_i X_i`.
fn first_pass(subkeys: &Subkeys, pairs: &mut [[[u8; BLOCK_BYTES]; 2]]) -> Block {
    let e00 = subkeys.offset(0, 0);
    pairs.iter_mut().zip(subkeys.offsets(PAIR_ROW)).fold(
        Block::ZERO,
        |sum, ([left, right], e1i)| {
            let (m, m_prime) = (Block::from_bytes(*left), Block::from_bytes(*right));
            let w = m ^ subkeys.aes4_at(e1i, m_prime);
            let x = m_prime ^ subkeys.aes4_at(e00, w);
            *left = w.to_bytes();
            *right = x.to_bytes();
            sum ^ x
        },
    )
}

/// Pass 2: `(W_i, X_i) ↦ (C_i, C'_i)` in place under `S`; returns `⊕_i Y_i`.
fn second_pass(subkeys: &Subkeys, pairs: &mut [[[u8; BLOCK_BYTES]; 2]], s: Block) -> Block {
    let e00 = subkeys.offset(0, 0);
    pairs
        .iter_mut()
        .zip(subkeys.offsets(SPREAD_ROW).zip(subkeys.offsets(PAIR_ROW)))
        .fold(Block::ZERO, |sum, ([left, right], (e2i, e1i))| {
            let spread = subkeys.aes4_at(e2i, s);
            let y = Block::from_bytes(*left) ^ spread;
            let z = Block::from_bytes(*right) ^ spread;
            let c_prime = y ^ subkeys.aes4_at(e00, z);
            let c = z ^ subkeys.aes4_at(e1i, c_prime);
            *left = c.to_bytes();
            *right = c_prime.to_bytes();
            sum ^ y
        })
}
