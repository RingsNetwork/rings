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
mod hash;
mod tbc;
#[cfg(test)]
mod tests;
mod typed;

use zeroize::Zeroize;

use crate::block::Block;
use crate::cipher::Direction;
pub use crate::error::DecryptError;
pub use crate::error::ExpansionExceedsBuffer;
pub use crate::error::Inauthentic;
pub use crate::error::KeyError;
pub use crate::error::Subkey;
use crate::tbc::Subkeys;
pub use crate::tbc::KEY_BYTES;
pub use crate::typed::Ciphertext;
pub use crate::typed::Plaintext;

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
        split_authenticator(buffer, expansion)?;
        self.encrypt_in_place(tweak, expansion, buffer);
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
        self.decrypt_in_place(tweak, expansion, buffer)?;
        Ok(split_authenticator(buffer, expansion)?.0)
    }

    /// `Encrypt_K(T, τ, M)` on an owned plaintext, whose type already reserves the `τ`-byte
    /// slot: [`Self::encrypt`] without its failure. The plaintext's buffer becomes the
    /// ciphertext's, so no copy of `M` outlives the call.
    pub fn seal<const TAU: usize>(
        &self,
        tweak: Tweak<'_>,
        plaintext: Plaintext<TAU>,
    ) -> Ciphertext<TAU> {
        let (mut bytes, message) = plaintext.into_slotted();
        self.encrypt_in_place(tweak, TAU, bytes.as_mut_slice());
        Ciphertext::from_sealed(bytes, message)
    }

    /// `Decrypt_K(T, τ, C)` on an owned ciphertext, whose type already guarantees `|C| ≥ τ`:
    /// [`Self::decrypt`] without its truncation failure.
    ///
    /// # Errors
    ///
    /// [`Inauthentic`] if the deciphered authenticator is not `0^τ`; the buffer is then zeroized
    /// and dropped.
    pub fn open<const TAU: usize>(
        &self,
        tweak: Tweak<'_>,
        ciphertext: Ciphertext<TAU>,
    ) -> Result<Plaintext<TAU>, Inauthentic> {
        let (mut bytes, message) = ciphertext.into_parts();
        self.decrypt_in_place(tweak, TAU, bytes.as_mut_slice())?;
        Ok(Plaintext::from_opened(bytes, message))
    }

    /// The one encryption core: `buffer ← Encipher(M ‖ 0^τ)` for `buffer = M ‖ slot`.
    ///
    /// Pre: `|buffer| ≥ τ`, established by each caller (checked, or by type).
    fn encrypt_in_place(&self, tweak: Tweak<'_>, expansion: usize, buffer: &mut [u8]) {
        buffer
            .iter_mut()
            .rev()
            .take(expansion)
            .for_each(|byte| *byte = 0);
        self.transform(tweak, expansion, Direction::Encipher, buffer);
    }

    /// The one decryption core: `buffer ← Decipher(C)`, accepted iff its last `τ` bytes are
    /// `0^τ`; on rejection the whole buffer is zeroized.
    ///
    /// Pre: `|buffer| ≥ τ`, established by each caller (checked, or by type).
    fn decrypt_in_place(
        &self,
        tweak: Tweak<'_>,
        expansion: usize,
        buffer: &mut [u8],
    ) -> Result<(), Inauthentic> {
        self.transform(tweak, expansion, Direction::Decipher, buffer);
        if is_zero(buffer.iter().rev().take(expansion)) {
            Ok(())
        } else {
            buffer.zeroize();
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

/// Splits `buffer = M ‖ A` with `|A| = τ`, by a checked subtraction and a checked split.
///
/// # Errors
///
/// [`ExpansionExceedsBuffer`] if the buffer is shorter than `τ`.
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
fn is_zero<'a>(bytes: impl Iterator<Item = &'a u8>) -> bool {
    let union = bytes.fold(0u8, |union, byte| union | byte);
    subtle::ConstantTimeEq::ct_eq(&union, &0).into()
}
