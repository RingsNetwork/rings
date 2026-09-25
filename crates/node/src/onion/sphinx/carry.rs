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
//! (L3, L5′). A value of type `()` is `ε`, so every consumer authenticates `pad(ε)`. Every
//! operation works in place on the slot of the cell's one buffer (zero-copy, #843): a relay
//! deciphers it where it arrived, and a consumer's value is a view into it
//! ([`OnionCarryValue`]), zeroized with the buffer.
//!
//! Laws:
//!
//! - **Round trip.** `open_{k_c} ∘ peel_{k_{r_s}} ∘ … ∘ peel_{k_{r_1}} ∘ seal_K = Right`.
//! - **L8.** Consecutive `y_{k,j}` differ, and a change of any `y_{k,j}` makes `open` reject the
//!   whole value (AEZ is a strong PRP: the change spreads over the whole block of every later
//!   decipherment), except with probability `2^−8τ + ε_AEZ`; a rejected value yields nothing.

use core::ops::Range;

use rings_aez::Tweak;
use zeroize::Zeroizing;

use super::class::OnionLoopClass;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;

/// `τ`, the consumer's authenticator width in bytes.
pub(crate) const ONION_CARRY_AUTHENTICATOR_BYTES: usize = 16;

/// The byte that ends a value inside its padding, `pad(v) = v ‖ 0x80 ‖ 0*`.
const PADDING_MARKER: u8 = 0x80;

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

/// An opened carry value `v`: a view into the buffer of the cell it arrived in, which is
/// zeroized on drop, so opening copies nothing and leaves no plaintext behind.
pub(crate) struct OnionCarryValue {
    /// The cell buffer, holding `v` at `range` after the in-place decryption.
    buffer: Zeroizing<Vec<u8>>,
    /// Where `v` lies in `buffer`.
    range: Range<usize>,
}

impl OnionCarryValue {
    /// `v`, the bytes the producer sealed.
    pub(crate) fn as_slice(&self) -> &[u8] {
        self.buffer.get(self.range.clone()).unwrap_or_default()
    }
}

impl core::fmt::Debug for OnionCarryValue {
    /// Redacts the value: it is plaintext of the loop.
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "OnionCarryValue({} bytes)", self.range.len())
    }
}

/// `seal_K(v) = Enc⁰_{k_{r_1}} ∘ … ∘ Enc⁰_{k_{r_s}} ∘ Enc^τ_{k_c}(pad(v))`, into the slot `y` of a
/// class-`b` cell, at the producer.
///
/// ```text
/// y[0, C₀) ← pad(v)                          |y| = C_b = C₀ + τ
/// y ← Enc^τ_{k_c}(y[0, C₀))                  innermost: the consumer's authenticated layer
/// for j = s, s−1, …, 1:  y ← Enc⁰_{k_{r_j}}(y)   so that r_1 peels first
/// ```
///
/// Pre: `|slot| = C_b` of `class`, which the cell constructors establish.
///
/// # Errors
///
/// [`OnionValueTooWide`] if `|v| ≥ C₀`; the slot is then untouched.
pub(super) fn seal(
    class: OnionLoopClass,
    keys: &OnionSegmentKeys,
    value: &[u8],
    slot: &mut [u8],
) -> Result<(), OnionValueTooWide> {
    let capacity = class.value_capacity();
    if value.len() > capacity {
        return Err(OnionValueTooWide {
            length: value.len(),
            capacity,
        });
    }
    slot.iter_mut()
        .zip(
            value
                .iter()
                .copied()
                .chain(core::iter::once(PADDING_MARKER))
                .chain(core::iter::repeat(0)),
        )
        .for_each(|(byte, padded)| *byte = padded);
    // `|slot| = C_b ≥ τ` by the class invariant, so `encrypt` has its authenticator room.
    let _ = keys
        .consumer()
        .aez()
        .encrypt(Tweak::EMPTY, ONION_CARRY_AUTHENTICATOR_BYTES, slot);
    keys.relays()
        .iter()
        .rev()
        .for_each(|relay| relay.aez().encipher(Tweak::EMPTY, slot));
    Ok(())
}

/// `peel_k(y) = Dec⁰_k(y)` in place, at a relay: one length-preserving layer off, never failing.
pub(super) fn peel(key: &OnionCarryKey, slot: &mut [u8]) {
    key.aez().decipher(Tweak::EMPTY, slot);
}

/// `open_k(y) = pad⁻¹(Dec^τ_k(y))` of the slot at `slot` in `buffer`, at the consumer, in place:
/// the value stays where it was deciphered, and the buffer is zeroized on drop.
///
/// # Errors
///
/// [`OnionOpenError::Inauthentic`] if the authenticator fails, rejecting the whole value (the
/// deciphered slot is zeroized by AEZ), and [`OnionOpenError::Padding`] if the message has no
/// padding marker.
pub(super) fn open(
    key: &OnionCarryKey,
    buffer: Vec<u8>,
    slot: Range<usize>,
) -> Result<OnionCarryValue, OnionOpenError> {
    let mut buffer = Zeroizing::new(buffer);
    let start = slot.start;
    let message = buffer
        .get_mut(slot)
        .ok_or(OnionOpenError::Inauthentic)
        .and_then(|slot| {
            key.aez()
                .decrypt(Tweak::EMPTY, ONION_CARRY_AUTHENTICATOR_BYTES, slot)
                .map_err(|_| OnionOpenError::Inauthentic)
        })?;
    let length = message
        .iter()
        .rposition(|byte| *byte != 0)
        .filter(|end| message.get(*end) == Some(&PADDING_MARKER))
        .ok_or(OnionOpenError::Padding)?;
    Ok(OnionCarryValue {
        buffer,
        range: start..start + length,
    })
}
