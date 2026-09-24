//! Sphinx cells of the onion Kleisli loop: the pure primitives of #834 Phase 2a (#840).
//!
//! Every edge of a loop of class `b` carries one fixed-width cell (#834 D6, D6″),
//!
//! ```text
//! cell = α ‖ β ‖ γ ‖ y       |α| = 33, |β| = Ĥ·ℓ, |γ| = 16, |y| = C_b = b − |χ| − F
//!        └── χ ──┘            Ĥ = MAX_ONION_LOOP_HOPS = 14, ℓ = 205, |χ| = 2919, F = 0
//! ```
//!
//! where the header `χ = (α, β, γ)` routes the cell (Sphinx, [`header`]) and the carry slot `y`
//! transports one segment value under nested AEZ layers ([`carry`]); [`cell`] parses a received
//! string into `(b, χ, y)` with `b` its observed length, the only source of `b`. A hop maps its
//! cell by
//!
//! ```text
//! peel_i : χ_i ↦ (λ_i, χ_{i+1})                  one header layer         (Def. header)
//! y_{k,j} = Dec⁰_{k_{r_{k,j}}}(y_{k,j−1})         one AEZ layer at a relay (D7)
//! b_k     = Dec^τ_{k_{c_k}}(y_{k,s})              authenticated at the consumer
//! ```
//!
//! with every key derived from a 32-byte seed in the hop's own layer `λ_i` ([`seed`]); `λ_i`
//! itself is the uniform layer of [`layer`], and [`class`] fixes `C_b` per loop class.
//!
//! Laws (discharged by `tests`, one module per law):
//!
//! - **Header correctness** (Prop. header correctness). For `1 ≤ H ≤ Ĥ` and every position
//!   `1 ≤ i ≤ H`, `i = H` (the guard facing the client, D6′) included, `peel_i` under hop `i`'s key
//!   yields `λ_i`, every `γ_i` verifies, and the client receives its tag `t_⋄ = γ_{H+1}`.
//! - **Class binding** (#834 H1). `γ_i` covers the class `b`, so a cell relabelled to another
//!   class fails `γ` at the next honest hop.
//! - **L5′ lengths.** `|χ_i| = |χ|` and `|y_i| = C_b` at every position and for every `H`: the
//!   lengths are functions of the class `b` alone, never of `H` or `i`.
//! - **L8 unlinkability.** `χ_i ≠ χ_{i+1}` and `y_{i−1} ≠ y_i` on every edge, and a one-bit change
//!   anywhere in a segment's carry makes the consumer reject the whole value.
//! - **D7 seeds.** Derived keys equal their HKDF definition, and a key with a zero AEZ subkey is
//!   detected, so the client re-draws the segment seed.
//!
//! Assumptions this module relies on, stated for the data-plane integration (#834 Phase 2a-4):
//!
//! 1. **No discriminant** (`F = 0`). A cell is exactly `b` bytes and its class is its length,
//!    bound by `γ = MAC(b ‖ β)`. Link cover traffic is a uniformly random `b`-byte cell, which the
//!    receiving hop rejects at `α` decoding (`≈ 99.6 %`, before any ECDH) or at `γ`; both are one
//!    outcome, [`header::OnionHeaderError::Invalid`], charged `u(b)` by admission (#834) and
//!    checked before any replay-store insertion (L9 counts admitted layers only).
//! 2. **Integrity and neighbour.** No per-edge cell AEAD remains: header integrity is `γ`, carry
//!    integrity is the consumer's AEZ authenticator, and the neighbour `from` is the authenticated
//!    transport link.
//!
//! Nothing here is wired to the data plane, and no live wire message changes: the module is
//! crate-private until #834 Phase 2a-4 (#843) uses it.

use hkdf::Hkdf;
use hkdf::HkdfExtract;
use sha2::Sha256;
use zeroize::Zeroize;
use zeroize::Zeroizing;

pub(crate) mod carry;
pub(crate) mod cell;
pub(crate) mod class;
pub(crate) mod header;
pub(crate) mod layer;
pub(crate) mod seed;
#[cfg(test)]
mod tests;

/// Relays per segment, `s` (#834 D5): the guard counts as a relay of both end segments.
pub(crate) const ONION_SEGMENT_RELAYS: usize = 2;

/// Upper bound `n_max` on the symbol hops of one loop (#834 D5).
pub(crate) const MAX_ONION_LOOP_SYMBOLS: usize = 4;

/// `Ĥ = H(n_max, s) = (s + 1)·n_max + s = 14`, the hop visits of the longest loop (#834 D5).
///
/// The fixed-length header has exactly `Ĥ` layer slots, whatever the loop's own `H` (L5′).
pub(crate) const MAX_ONION_LOOP_HOPS: usize =
    (ONION_SEGMENT_RELAYS + 1) * MAX_ONION_LOOP_SYMBOLS + ONION_SEGMENT_RELAYS;

/// `target ← target ⊕ mask`, bytewise over the common prefix of the two streams.
fn xor_in_place<'a, 'b>(
    target: impl IntoIterator<Item = &'a mut u8>,
    mask: impl IntoIterator<Item = &'b u8>,
) {
    target
        .into_iter()
        .zip(mask)
        .for_each(|(byte, mask)| *byte ^= mask);
}

/// `HKDF-Expand(PRK, info₁ ‖ … ‖ infoₙ)[0, N)`, zeroized on drop.
///
/// HKDF-Expand is defined exactly for `N ≤ 255·|SHA-256|`, which is asserted at compile time, so
/// its only error, `InvalidLength`, cannot occur and the result is discarded.
fn hkdf_expand<const N: usize>(kdf: &Hkdf<Sha256>, info: &[&[u8]]) -> Zeroizing<[u8; N]> {
    const { assert!(N <= 255 * 32) };
    let mut okm = Zeroizing::new([0_u8; N]);
    let _ = kdf.expand_multi_info(info, okm.as_mut_slice());
    okm
}

/// `HKDF-Extract(salt, ikm₁ ‖ … ‖ ikmₙ)`, with the pseudorandom key the extract step returns
/// wiped at once.
///
/// The PRK is also held inside the returned `Hkdf`'s HMAC state, which hkdf 0.12 and hmac 0.12
/// cannot wipe; that residue is tracked in its own issue.
fn hkdf_extract(salt: &[u8], ikm: &[&[u8]]) -> Hkdf<Sha256> {
    let mut extract = HkdfExtract::<Sha256>::new(Some(salt));
    ikm.iter().for_each(|part| extract.input_ikm(part));
    let (mut prk, kdf) = extract.finalize();
    prk.zeroize();
    kdf
}
