//! Carry segments: one value through `s` relays under nested AEZ layers (#834 D7, L8).
//!
//! Write `Enc^τ_k`, `Dec^τ_k` for AEZ v5 with the empty tweak (no nonce, no associated data).
//! For segment `k`, from producer `p_k` through relays `r_{k,1} … r_{k,s}` to consumer `c_k`,
//!
//! ```text
//! b_k     = pad(v_k) = v_k ‖ 0x80 ‖ 0^{C₀ − |v_k| − 1}                      (L3)
//! y_{k,0} = Enc⁰_{k_{r_1}} ∘ … ∘ Enc⁰_{k_{r_s}} ∘ Enc^τ_{k_c}(b_k)          seal, at p_k
//! y_{k,j} = Dec⁰_{k_{r_j}}(y_{k,j−1})                                         peel, at r_{k,j}
//! b_k     = Dec^τ_{k_c}(y_{k,s}),  v_k = pad⁻¹(b_k)                          open, at c_k
//! ```
//!
//! with `τ = 16`. Every layer preserves the width `C_b`, so the slot has one width on every edge
//! (L3, L5′). A value of type `()` is `ε`, so every consumer authenticates `pad(ε)`.
//!
//! Laws:
//!
//! - **Round trip.** `open_{k_c} ∘ peel_{k_{r_s}} ∘ … ∘ peel_{k_{r_1}} ∘ seal_K = Right`.
//! - **L8.** Consecutive `y_{k,j}` differ, and a change of any `y_{k,j}` makes `open` reject the
//!   whole value (AEZ is a strong PRP: the change spreads over the whole block of every later
//!   decipherment), except with probability `2^−8τ + ε_AEZ`; a rejected value yields nothing.

use rings_aez::Tweak;

use super::class::OnionLoopClass;
use super::seed::OnionCarryKey;
use super::seed::OnionSegmentKeys;

/// `τ`, the consumer's authenticator width in bytes.
pub(crate) const ONION_CARRY_AUTHENTICATOR_BYTES: usize = 16;

/// The byte that ends a value inside its padding, `pad(v) = v ‖ 0x80 ‖ 0*`.
const PADDING_MARKER: u8 = 0x80;

/// The carry slot `y` of one cell of a class-`b` loop.
///
/// Invariant: `|bytes| = C_b` for the loop's class, established by every constructor; the class
/// is the slot length, so it is not stored beside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionCarry {
    /// The slot, exactly `C_b` bytes.
    bytes: Vec<u8>,
}

/// Why a carry was not produced or not accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionCarryError {
    /// The slot is not `C_b` bytes long.
    #[error("carry of {length} bytes is not the {expected}-byte slot of its class")]
    Width {
        /// The slot length.
        length: usize,
        /// `C_b`.
        expected: usize,
    },
    /// The value does not fit `pad`: `|v| ≥ C₀` (L3 fails closed, never truncates).
    #[error("carry value of {length} bytes exceeds the {capacity}-byte capacity of its class")]
    ValueTooWide {
        /// `|v|`.
        length: usize,
        /// `C₀ − 1`, the widest value `pad` admits.
        capacity: usize,
    },
    /// The consumer's AEZ decryption rejected the slot; nothing of it is released (L8).
    #[error("carry authenticator does not verify")]
    Inauthentic,
    /// The authenticated plaintext is not in the image of `pad`.
    #[error("carry value has no padding marker")]
    Padding,
}

impl OnionCarry {
    /// Accept a received slot of a class-`b` cell.
    ///
    /// # Errors
    ///
    /// [`OnionCarryError::Width`] unless `|bytes| = C_b`.
    pub(crate) fn from_bytes(
        class: OnionLoopClass,
        bytes: Vec<u8>,
    ) -> Result<Self, OnionCarryError> {
        if bytes.len() == class.carry_bytes() {
            Ok(Self { bytes })
        } else {
            Err(OnionCarryError::Width {
                length: bytes.len(),
                expected: class.carry_bytes(),
            })
        }
    }

    /// Return the slot bytes, `C_b` of them.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// `seal_K(v) = Enc⁰_{k_{r_1}} ∘ … ∘ Enc⁰_{k_{r_s}} ∘ Enc^τ_{k_c}(pad(v))`, at the producer.
    ///
    /// ```text
    /// y ← pad(v) ‖ 0^τ                        |y| = C_b
    /// y ← Enc^τ_{k_c}(y)                      innermost: the consumer's authenticated layer
    /// for j = s, s−1, …, 1:  y ← Enc⁰_{k_{r_j}}(y)   so that r_1 peels first
    /// ```
    ///
    /// # Errors
    ///
    /// [`OnionCarryError::ValueTooWide`] if `|v| ≥ C₀`. The slot is built with width
    /// `C_b ≥ 32 > τ`, so AEZ's only encryption error, a slot shorter than `τ`, is reported as the
    /// [`OnionCarryError::Width`] it would be.
    pub(crate) fn seal(
        class: OnionLoopClass,
        keys: &OnionSegmentKeys,
        value: &[u8],
    ) -> Result<Self, OnionCarryError> {
        let capacity = class.carry_value_bytes().saturating_sub(1);
        if value.len() > capacity {
            return Err(OnionCarryError::ValueTooWide {
                length: value.len(),
                capacity,
            });
        }
        let mut bytes = value
            .iter()
            .copied()
            .chain(core::iter::once(PADDING_MARKER))
            .chain(core::iter::repeat(0))
            .take(class.carry_bytes())
            .collect::<Vec<_>>();
        keys.consumer()
            .aez()
            .encrypt(
                Tweak::EMPTY,
                ONION_CARRY_AUTHENTICATOR_BYTES,
                bytes.as_mut_slice(),
            )
            .map_err(|short| OnionCarryError::Width {
                length: short.length,
                expected: class.carry_bytes(),
            })?;
        keys.relays()
            .iter()
            .rev()
            .for_each(|relay| relay.aez().encipher(Tweak::EMPTY, bytes.as_mut_slice()));
        Ok(Self { bytes })
    }

    /// `peel_k(y) = Dec⁰_k(y)`, at a relay: one length-preserving layer off, never failing.
    pub(crate) fn peel(mut self, key: &OnionCarryKey) -> Self {
        key.aez().decipher(Tweak::EMPTY, self.bytes.as_mut_slice());
        self
    }

    /// `open_k(y) = pad⁻¹(Dec^τ_k(y))`, at the consumer.
    ///
    /// # Errors
    ///
    /// [`OnionCarryError::Inauthentic`] if AEZ rejects the slot, rejecting the whole value, and
    /// [`OnionCarryError::Padding`] if the plaintext has no padding marker.
    pub(crate) fn open(mut self, key: &OnionCarryKey) -> Result<Vec<u8>, OnionCarryError> {
        let padded = key
            .aez()
            .decrypt(
                Tweak::EMPTY,
                ONION_CARRY_AUTHENTICATOR_BYTES,
                self.bytes.as_mut_slice(),
            )
            .map_err(|_| OnionCarryError::Inauthentic)?;
        let marker = padded
            .iter()
            .copied()
            .rposition(|byte| byte != 0)
            .filter(|end| padded.get(*end) == Some(&PADDING_MARKER))
            .ok_or(OnionCarryError::Padding)?;
        self.bytes.truncate(marker);
        Ok(self.bytes)
    }
}
