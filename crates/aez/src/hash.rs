//! AEZ-hash and AEZ-prf (AEZ v5, §3): the tweak digest `Δ` and the keystream used when the
//! message is empty.

use crate::block::Block;
use crate::block::BLOCK_BYTES;
use crate::tbc::Subkeys;

/// Row of `E` used by the first hashed component: component `T_i` (from 1) uses
/// `j = i + 2`.
const FIRST_COMPONENT_ROW: u128 = 3;

/// Column of `E^{−1,·}` that generates the AEZ-prf keystream.
const PRF_COLUMN: u128 = 3;

/// `Δ = AEZ-hash_K(T_1, …, T_t) = ⊕_i H_{i+2}(T_i)`.
///
/// AEZ-hash is a universal hash of the component *vector*: each component is digested
/// under its own row `j` of `E`, so reordering or re-splitting components changes the
/// hashed function, not only its input.
pub(crate) fn hash<'a>(subkeys: &Subkeys, components: impl Iterator<Item = &'a [u8]>) -> Block {
    components
        .zip(FIRST_COMPONENT_ROW..)
        .fold(Block::ZERO, |delta, (component, row)| {
            delta ^ component_digest(subkeys, row, component)
        })
}

/// `H_j(T)` for one component `T = X_1 ‖ … ‖ X_m` split into 16-byte blocks:
///
/// ```text
/// T = ε                       ↦  E^{j,0}(10*)
/// |X_m| = 128 (T aligned)     ↦  ⊕_{l=1..m}   E^{j,l}(X_l)
/// |X_m| < 128                 ↦  ⊕_{l=1..m−1} E^{j,l}(X_l) ⊕ E^{j,0}(X_m 10*)
/// ```
fn component_digest(subkeys: &Subkeys, row: u128, component: &[u8]) -> Block {
    let (full, partial) = component.as_chunks::<BLOCK_BYTES>();
    let body = full
        .iter()
        .zip(subkeys.offsets(row))
        .fold(Block::ZERO, |digest, (block, offset)| {
            digest ^ subkeys.aes4_at(offset, Block::from_bytes(*block))
        });
    let padded_tail = !partial.is_empty() || full.is_empty();
    if padded_tail {
        body ^ subkeys.e(row, 0, Block::padded(partial))
    } else {
        body
    }
}

/// `buffer ⊕= AEZ-prf_K(Δ, |buffer|)`, the first `|buffer|` bytes of
/// `E^{−1,3}(Δ ⊕ [0]) ‖ E^{−1,3}(Δ ⊕ [1]) ‖ …`.
///
/// XOR-ing instead of writing makes the map an involution, so one function serves both
/// sides of the empty-message case: encryption applies it to `0^τ` (yielding the tag) and
/// decryption to the tag (yielding `0^τ` exactly when the tag is valid).
pub(crate) fn xor_prf(subkeys: &Subkeys, delta: Block, buffer: &mut [u8]) {
    buffer
        .chunks_mut(BLOCK_BYTES)
        .zip(0u128..)
        .for_each(|(chunk, counter)| {
            subkeys
                .e_aes10(PRF_COLUMN, delta ^ Block::from_index(counter))
                .xor_into(chunk)
        });
}
