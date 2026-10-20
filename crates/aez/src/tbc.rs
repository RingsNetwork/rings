//! The tweakable blockcipher `E_K^{j,i} : Block → Block` (AEZ v5, §3).
//!
//! The key `K = I ‖ J ‖ L ∈ ({0,1}^128)^3` determines two AES-round families,
//!
//! ```text
//! AES4_K  = R_0 ∘ R_L ∘ R_I ∘ R_J                     (R_k = one AES round, key k)
//! AES10_K = R_I ∘ R_L ∘ R_J ∘ R_I ∘ R_L ∘ R_J ∘ R_I ∘ R_L ∘ R_J ∘ R_I
//! ```
//!
//! (application order right to left; every round includes MixColumns), and the family
//!
//! ```text
//! E^{−1,i}(X) = AES10(X ⊕ i·L)
//! E^{j,i}(X)  = AES4(X ⊕ Δ_{j,i}),  Δ_{j,i} = j·J ⊕ 2^⌈i/8⌉·I ⊕ (i mod 8)·L   (j ≥ 0)
//! ```
//!
//! AEZ-core and AEZ-hash evaluate `E^{j,i}` for `i = 1, 2, …` in order, so
//! [`Subkeys::offsets`] produces `Δ_{j,1}, Δ_{j,2}, …` by one doubling every eight
//! indices instead of `⌈i/8⌉` doublings per block.
//!
//! The rounds are `aes::hazmat::cipher_round`: AES-NI on x86, fixsliced (table-free,
//! constant-time) software AES elsewhere, including `wasm32`.

use zeroize::Zeroize;
use zeroize::ZeroizeOnDrop;

use crate::block::Block;
use crate::block::BLOCK_BYTES;
use crate::error::KeyError;
use crate::error::Subkey;

/// Key length in bytes: `|K| = 384` bits, for which AEZ's `Extract` is the identity.
pub const KEY_BYTES: usize = 3 * BLOCK_BYTES;

/// The three subkeys `(I, J, L)` of a 384-bit AEZ key.
///
/// Invariant: `I ≠ 0 ∧ J ≠ 0 ∧ L ≠ 0`, established by [`Subkeys::new`]. Zeroized on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct Subkeys {
    /// `I = K[1..128]`.
    i: Block,
    /// `J = K[129..256]`.
    j: Block,
    /// `L = K[257..384]`.
    l: Block,
}

impl Subkeys {
    /// Splits `K = I ‖ J ‖ L`, rejecting a key with a zero subkey.
    ///
    /// A zero `I`, `J` or `L` collapses the offsets `Δ_{j,i}` (Mennink, "Weak keys for
    /// AEZ", CT-RSA 2017); a uniformly drawn key hits one with probability `≈ 3·2^−128`.
    pub(crate) fn new(key: &[u8; KEY_BYTES]) -> Result<Self, KeyError> {
        let [i, j, l] = core::array::from_fn(|index| {
            let start = index.saturating_mul(BLOCK_BYTES);
            // `get` is total; the zero fallback is unreachable for a 48-byte key and would
            // be rejected as a weak key below, so it fails closed.
            key.get(start..start.saturating_add(BLOCK_BYTES))
                .map_or(Block::ZERO, Block::from_prefix)
        });
        let subkeys = Self { i, j, l };
        subkeys
            .zero_subkey()
            .map_or(Ok(subkeys), |name| Err(KeyError::ZeroSubkey(name)))
    }

    /// The first subkey that is `0^128`, if any.
    fn zero_subkey(&self) -> Option<Subkey> {
        [
            (Subkey::I, self.i),
            (Subkey::J, self.j),
            (Subkey::L, self.l),
        ]
        .into_iter()
        .find_map(|(name, block)| block.is_zero().then_some(name))
    }

    /// `AES4_K(X)`: four rounds under `(J, I, L, 0)`.
    fn aes4(&self, block: Block) -> Block {
        [self.j, self.i, self.l, Block::ZERO]
            .into_iter()
            .fold(block, aes_round)
    }

    /// `AES10_K(X)`: ten rounds under `(I, J, L, I, J, L, I, J, L, I)`.
    fn aes10(&self, block: Block) -> Block {
        [self.i, self.j, self.l]
            .into_iter()
            .cycle()
            .take(10)
            .fold(block, aes_round)
    }

    /// `Δ_{j,i} = j·J ⊕ 2^⌈i/8⌉·I ⊕ (i mod 8)·L`.
    pub(crate) fn offset(&self, j: u128, i: u32) -> Block {
        self.j.times(j) ^ self.i.times_power_of_two(i.div_ceil(8)) ^ self.l.times(u128::from(i % 8))
    }

    /// The stream `Δ_{j,1}, Δ_{j,2}, Δ_{j,3}, …`.
    ///
    /// State: `(i, 2^⌈i/8⌉·I)`, initially `(1, 2·I)`. Step: `i ↦ i + 1`, and the power of
    /// two doubles exactly when `8 | i`, the indices at which `⌈i/8⌉` increases.
    /// Law: the `k`-th element (from 1) equals `offset(j, k)`.
    pub(crate) fn offsets(&self, j: u128) -> impl Iterator<Item = Block> + '_ {
        let j_part = self.j.times(j);
        core::iter::successors(Some((1u64, self.i.double())), |&(i, power)| {
            let next = if i.is_multiple_of(8) {
                power.double()
            } else {
                power
            };
            Some((i.wrapping_add(1), next))
        })
        .map(move |(i, power)| j_part ^ power ^ self.l.times(u128::from(i % 8)))
    }

    /// `E^{j,i}(X)` for `j ≥ 0` at a precomputed offset `Δ_{j,i}`.
    pub(crate) fn aes4_at(&self, offset: Block, block: Block) -> Block {
        self.aes4(block ^ offset)
    }

    /// `E^{j,i}(X)` for `j ≥ 0`.
    pub(crate) fn e(&self, j: u128, i: u32, block: Block) -> Block {
        self.aes4_at(self.offset(j, i), block)
    }

    /// `E^{−1,i}(X) = AES10(X ⊕ i·L)`.
    pub(crate) fn e_aes10(&self, i: u128, block: Block) -> Block {
        self.aes10(block ^ self.l.times(i))
    }
}

/// One AES encryption round, `AESENC` semantics: SubBytes, ShiftRows, MixColumns, then
/// `⊕ key`.
fn aes_round(state: Block, key: Block) -> Block {
    let mut block = aes::Block::from(state.to_bytes());
    aes::hazmat::cipher_round(&mut block, &aes::Block::from(key.to_bytes()));
    Block::from_bytes(block.into())
}
