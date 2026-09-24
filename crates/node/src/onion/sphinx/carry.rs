//! Carry segments: one value through `s` relays under nested AEZ layers (#834 D7, L8).
//!
//! Write `Enc^τ_k`, `Dec^τ_k` for AEZ v5 with the empty tweak (no nonce, no associated data).
//! For segment `k`, from producer `p_k` through relays `r_{k,1} … r_{k,s}` to consumer `c_k`,
//!
//! ```text
//! m_k     = pad(v_k) = v_k ‖ 0x80 ‖ 0^{C₀ − |v_k| − 1}                     (L3; padded message)
//! y_{k,0} = Enc⁰_{k_{r_1}} ∘ … ∘ Enc⁰_{k_{r_s}} ∘ Enc^τ_{k_c}(m_k)         seal, at p_k
//! y_{k,j} = Dec⁰_{k_{r_j}}(y_{k,j−1})                                        peel, at r_{k,j}
//! m_k     = Dec^τ_{k_c}(y_{k,s}),  v_k = pad⁻¹(m_k)                         open, at c_k
//! ```
//!
//! with `τ = 16`. Every layer preserves the width `C_b`, so the slot has one width on every edge
//! (L3, L5′). A value of type `()` is `ε`, so every consumer authenticates `pad(ε)`. The slot is a
//! `rings_aez` [`Ciphertext`], so `open` needs no length check, and a value is only ever held as
//! a zeroizing [`Plaintext`].
//!
//! Laws:
//!
//! - **Round trip.** `open_{k_c} ∘ peel_{k_{r_s}} ∘ … ∘ peel_{k_{r_1}} ∘ seal_K = Right`.
//! - **L8.** Consecutive `y_{k,j}` differ, and a change of any `y_{k,j}` makes `open` reject the
//!   whole value (AEZ is a strong PRP: the change spreads over the whole block of every later
//!   decipherment), except with probability `2^−8τ + ε_AEZ`; a rejected value yields nothing.

use rings_aez::Ciphertext;
use rings_aez::Plaintext;
use rings_aez::Tweak;
use zeroize::Zeroizing;

use super::class::OnionLoopClass;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;

/// `τ`, the consumer's authenticator width in bytes.
pub(crate) const ONION_CARRY_AUTHENTICATOR_BYTES: usize = 16;

/// The byte that ends a value inside its padding, `pad(v) = v ‖ 0x80 ‖ 0*`.
const PADDING_MARKER: u8 = 0x80;

/// A carry slot `y`: an AEZ ciphertext with a `τ`-byte authenticator under its outer layers.
pub(crate) type OnionCarry = Ciphertext<ONION_CARRY_AUTHENTICATOR_BYTES>;

/// A value that does not fit `pad`: `|v| ≥ C₀` (L3 fails closed, never truncates).
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("carry value of {length} bytes exceeds the {capacity}-byte capacity of its class")]
pub(crate) struct OnionValueTooWide {
    /// `|v|`.
    pub(crate) length: usize,
    /// `C₀ − 1`, the widest value `pad` admits.
    pub(crate) capacity: usize,
}

/// Why a consumer rejected a carry slot; nothing of it is released either way (L8).
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionOpenError {
    /// The authenticator does not verify.
    #[error("carry authenticator does not verify")]
    Inauthentic,
    /// The authenticated message is not in the image of `pad`.
    #[error("carry value has no padding marker")]
    Padding,
}

/// `seal_K(v) = Enc⁰_{k_{r_1}} ∘ … ∘ Enc⁰_{k_{r_s}} ∘ Enc^τ_{k_c}(pad(v))`, at the producer.
///
/// ```text
/// m ← pad(v)                              |m| = C₀, so |Enc^τ(m)| = C_b
/// y ← Enc^τ_{k_c}(m)                      innermost: the consumer's authenticated layer
/// for j = s, s−1, …, 1:  y ← Enc⁰_{k_{r_j}}(y)   so that r_1 peels first
/// ```
///
/// # Errors
///
/// [`OnionValueTooWide`] if `|v| ≥ C₀`.
pub(super) fn seal(
    class: OnionLoopClass,
    keys: &OnionSegmentKeys,
    value: &[u8],
) -> Result<OnionCarry, OnionValueTooWide> {
    let capacity = class.carry_value_bytes() - 1;
    if value.len() > capacity {
        return Err(OnionValueTooWide {
            length: value.len(),
            capacity,
        });
    }
    // `pad(v)` in a buffer with room for the slot, which `Plaintext::new` then keeps as it is.
    let mut padded = Vec::with_capacity(class.carry_bytes());
    padded.extend(
        value
            .iter()
            .copied()
            .chain(core::iter::once(PADDING_MARKER))
            .chain(core::iter::repeat(0))
            .take(class.carry_value_bytes()),
    );
    let message = Plaintext::new(padded);
    let mut carry = keys.consumer().aez().seal(Tweak::EMPTY, message);
    keys.relays()
        .iter()
        .rev()
        .for_each(|relay| relay.aez().encipher(Tweak::EMPTY, carry.as_mut_slice()));
    Ok(carry)
}

/// `peel_k(y) = Dec⁰_k(y)` in place, at a relay: one length-preserving layer off, never failing.
pub(super) fn peel(key: &OnionCarryKey, carry: &mut OnionCarry) {
    key.aez().decipher(Tweak::EMPTY, carry.as_mut_slice());
}

/// `open_k(y) = pad⁻¹(Dec^τ_k(y))`, at the consumer; the value is zeroized on drop.
///
/// # Errors
///
/// [`OnionOpenError::Inauthentic`] if the authenticator fails, rejecting the whole value, and
/// [`OnionOpenError::Padding`] if the message has no padding marker.
pub(super) fn open(
    key: &OnionCarryKey,
    carry: OnionCarry,
) -> Result<Zeroizing<Vec<u8>>, OnionOpenError> {
    let mut value = key
        .aez()
        .open(Tweak::EMPTY, carry)
        .map_err(|_| OnionOpenError::Inauthentic)?
        .into_zeroizing();
    let marker = value
        .iter()
        .copied()
        .rposition(|byte| byte != 0)
        .filter(|end| value.get(*end) == Some(&PADDING_MARKER))
        .ok_or(OnionOpenError::Padding)?;
    value.truncate(marker);
    Ok(value)
}
