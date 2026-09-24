#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]
#![doc = include_str!("../README.md")]

mod aez_core;
mod aez_tiny;
mod block;
mod cipher;
mod error;
mod expanded;
mod hash;
mod tbc;
#[cfg(test)]
mod tests;

use zeroize::Zeroize;

use crate::block::Block;
use crate::cipher::Direction;
pub use crate::error::DecryptError;
pub use crate::error::ExpansionExceedsBuffer;
pub use crate::error::Inauthentic;
pub use crate::error::KeyError;
pub use crate::error::Subkey;
pub use crate::expanded::Expanded;
use crate::tbc::Subkeys;
pub use crate::tbc::KEY_BYTES;

/// The tweak `T = (N, A_1, …, A_t)`: a nonce followed by associated-data strings.
///
/// AEZ hashes the tweak as the vector `([8τ]_128, N, A_1, …, A_t)`; the nonce is simply
/// the first component. Every component is an arbitrary byte string, including `ε`, and
/// component boundaries are significant: `(A ‖ B)` and `(A, B)` are different tweaks.
#[derive(Clone, Copy)]
pub struct Tweak<'a> {
    /// `(N, A_1, …, A_t)`.
    components: &'a [&'a [u8]],
}

impl<'a> Tweak<'a> {
    /// The empty tweak `()`: no nonce and no associated data. Sound when the key is used
    /// for a single message, as for a per-layer onion key.
    pub const EMPTY: Tweak<'static> = Tweak { components: &[] };

    /// The tweak `(components[0], components[1], …)`; by convention the nonce comes first.
    pub const fn new(components: &'a [&'a [u8]]) -> Self {
        Self { components }
    }
}

/// AEZ v5 under one 384-bit key: robust authenticated encryption with an arbitrary
/// expansion `τ`, and at `τ = 0` a length-preserving tweakable wide-block permutation.
///
/// Every operation works in place on the caller's buffer. The subkeys are zeroized on drop.
pub struct Aez {
    /// `(I, J, L)`.
    subkeys: Subkeys,
}

impl Aez {
    /// Keys AEZ with `K = I ‖ J ‖ L`.
    ///
    /// AEZ's `Extract` is the identity on 384-bit keys, so the key must already be
    /// uniformly random; there is no password-style derivation.
    ///
    /// # Errors
    ///
    /// [`KeyError::ZeroSubkey`] if `I`, `J` or `L` is `0^128`.
    pub fn new(key: &[u8; KEY_BYTES]) -> Result<Self, KeyError> {
        Subkeys::new(key).map(|subkeys| Self { subkeys })
    }

    /// `Encrypt_K(T, τ, M)` in place.
    ///
    /// The buffer holds `M ‖ slot` where `slot` is its last `τ = expansion` bytes; the slot's
    /// input content is ignored (it becomes `0^τ` before enciphering). On return the buffer
    /// holds the ciphertext `C`, with `|C| = |M| + τ`.
    ///
    /// ```text
    /// M = ε  ↦  C = AEZ-prf_K(Δ, τ)
    /// M ≠ ε  ↦  C = Encipher_K^Δ(M ‖ 0^τ)          Δ = AEZ-hash_K([8τ]_128, T)
    /// ```
    ///
    /// # Errors
    ///
    /// [`ExpansionExceedsBuffer`] if the buffer is shorter than `τ`; the buffer is untouched.
    pub fn encrypt(
        &self,
        tweak: Tweak<'_>,
        expansion: usize,
        buffer: &mut [u8],
    ) -> Result<(), ExpansionExceedsBuffer> {
        let (_, slot) = split_authenticator(buffer, expansion)?;
        slot.zeroize();
        self.transform(tweak, expansion, Direction::Encipher, buffer);
        Ok(())
    }

    /// `Decrypt_K(T, τ, C)` in place; returns the plaintext prefix of the buffer.
    ///
    /// Law: `decrypt(T, τ, encrypt(T, τ, M ‖ s)) = M` for every slot content `s`.
    ///
    /// # Errors
    ///
    /// - [`DecryptError::Truncated`] if `|C| < τ`; the buffer is untouched.
    /// - [`DecryptError::Inauthentic`] if the deciphered authenticator is not `0^τ`. The
    ///   whole buffer is zeroized, so no unverified plaintext is released.
    pub fn decrypt<'b>(
        &self,
        tweak: Tweak<'_>,
        expansion: usize,
        buffer: &'b mut [u8],
    ) -> Result<&'b mut [u8], DecryptError> {
        split_authenticator(buffer, expansion)?;
        self.transform(tweak, expansion, Direction::Decipher, buffer);
        let (plaintext, authenticator) = split_authenticator(buffer, expansion)?;
        if is_zero(authenticator) {
            Ok(plaintext)
        } else {
            plaintext.zeroize();
            authenticator.zeroize();
            Err(DecryptError::Inauthentic)
        }
    }

    /// `Encrypt_K(T, τ, M)` in place on a buffer whose width `|M| + τ ≥ τ` is already a type:
    /// [`Self::encrypt`] without its only failure.
    pub fn encrypt_expanded<const TAU: usize>(&self, tweak: Tweak<'_>, buffer: &mut Expanded<TAU>) {
        buffer.authenticator_mut().zeroize();
        self.transform(tweak, TAU, Direction::Encipher, buffer.as_mut_slice());
    }

    /// `Decrypt_K(T, τ, C)` in place on a buffer of width at least `τ`: [`Self::decrypt`] without
    /// its truncation failure. On success the message is [`Expanded::into_message`].
    ///
    /// # Errors
    ///
    /// [`Inauthentic`] if the deciphered authenticator is not `0^τ`; the whole buffer has then
    /// been zeroized.
    pub fn decrypt_expanded<const TAU: usize>(
        &self,
        tweak: Tweak<'_>,
        buffer: &mut Expanded<TAU>,
    ) -> Result<(), Inauthentic> {
        self.transform(tweak, TAU, Direction::Decipher, buffer.as_mut_slice());
        if is_zero(buffer.authenticator_mut()) {
            Ok(())
        } else {
            buffer.as_mut_slice().zeroize();
            Err(Inauthentic)
        }
    }

    /// `π_{K,T}`: `encrypt` at `τ = 0`, a length-preserving permutation of every length
    /// class `{0,1}^{8n}` (the identity on `ε`), indexed by the tweak.
    pub fn encipher(&self, tweak: Tweak<'_>, buffer: &mut [u8]) {
        self.transform(tweak, 0, Direction::Encipher, buffer);
    }

    /// `π_{K,T}^{−1}`: `decrypt` at `τ = 0`, which authenticates nothing and so never fails.
    ///
    /// Law: `decipher(T, encipher(T, X)) = X` and `encipher(T, decipher(T, X)) = X`.
    pub fn decipher(&self, tweak: Tweak<'_>, buffer: &mut [u8]) {
        self.transform(tweak, 0, Direction::Decipher, buffer);
    }

    /// The shared body of `Encrypt` and `Decrypt`: computes `Δ` and applies AEZ-prf when the
    /// message is empty (`|buffer| = τ`), `Encipher`/`Decipher` otherwise.
    fn transform(
        &self,
        tweak: Tweak<'_>,
        expansion: usize,
        direction: Direction,
        buffer: &mut [u8],
    ) {
        let expansion_bits = Block::from_index((expansion as u128) << 3).to_bytes();
        let components =
            core::iter::once(expansion_bits.as_slice()).chain(tweak.components.iter().copied());
        let delta = hash::hash(&self.subkeys, components);
        if buffer.len() == expansion {
            hash::xor_prf(&self.subkeys, delta, buffer);
        } else {
            cipher::apply(&self.subkeys, delta, direction, buffer);
        }
    }
}

/// Splits `buffer = M ‖ A` with `|A| = τ`.
fn split_authenticator(
    buffer: &mut [u8],
    expansion: usize,
) -> Result<(&mut [u8], &mut [u8]), ExpansionExceedsBuffer> {
    let length = buffer.len();
    length
        .checked_sub(expansion)
        .and_then(|message| buffer.split_at_mut_checked(message))
        .ok_or(ExpansionExceedsBuffer { length, expansion })
}

/// Whether every byte is zero, in time independent of the bytes' values.
fn is_zero(bytes: &[u8]) -> bool {
    let union = bytes.iter().fold(0u8, |union, byte| union | byte);
    subtle::ConstantTimeEq::ct_eq(&union, &0).into()
}
