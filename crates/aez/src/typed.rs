//! Owned AEZ buffers with the length precondition and the plaintext/ciphertext distinction as
//! types: `Plaintext<τ> → Ciphertext<τ>` by [`crate::Aez::seal`], and back by
//! [`crate::Aez::open`].
//!
//! ```text
//! Plaintext<τ>  ≅ { M : a buffer with τ bytes of reserved slot }      wiped on drop, opaque
//! Ciphertext<τ> ≅ { C : |C| ≥ τ }                                     public data
//! seal : Plaintext<τ> → Ciphertext<τ>             total
//! open : Ciphertext<τ> → Plaintext<τ> + Inauthentic
//! open ∘ seal = Right                             (on equal key and tweak)
//! ```
//!
//! A ciphertext cannot be read as a message: only `open` produces a [`Plaintext`]. A plaintext
//! has no `Clone`, `Debug` or equality, and its buffer is zeroized when dropped.

use zeroize::Zeroizing;

use crate::error::ExpansionExceedsBuffer;

/// An AEZ ciphertext `C` with `|C| ≥ τ`.
///
/// Invariant: `|bytes| ≥ TAU`, established by [`Ciphertext::new`] and by [`crate::Aez::seal`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ciphertext<const TAU: usize> {
    /// `C`.
    bytes: Vec<u8>,
}

/// An AEZ plaintext `M`, with capacity for its `τ`-byte authenticator slot; zeroized on drop.
pub struct Plaintext<const TAU: usize> {
    /// `M`, with capacity `|M| + τ`.
    bytes: Zeroizing<Vec<u8>>,
}

impl<const TAU: usize> Ciphertext<TAU> {
    /// Accept a received ciphertext.
    ///
    /// # Errors
    ///
    /// [`ExpansionExceedsBuffer`] if `|bytes| < τ`: no message is encrypted in it.
    pub fn new(bytes: Vec<u8>) -> Result<Self, ExpansionExceedsBuffer> {
        if bytes.len() >= TAU {
            Ok(Self { bytes })
        } else {
            Err(ExpansionExceedsBuffer {
                length: bytes.len(),
                expansion: TAU,
            })
        }
    }

    /// A buffer [`crate::Aez::seal`] has just enciphered, `|M| + τ` bytes long.
    pub(crate) const fn from_sealed(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Return `C`.
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Return `C` for a length-preserving transformation, such as a `τ = 0` layer on top.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.bytes.as_mut_slice()
    }

    /// Return `C`.
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }

    /// `C` for in-place decryption, wiped if it is dropped before release.
    pub(crate) fn into_zeroizing(self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.bytes)
    }
}

impl<const TAU: usize> Plaintext<TAU> {
    /// Take a message, moving it into a buffer with room for the slot; the caller's buffer is
    /// wiped.
    pub fn new(message: Vec<u8>) -> Self {
        let message = Zeroizing::new(message);
        let mut bytes = Zeroizing::new(Vec::with_capacity(message.len().saturating_add(TAU)));
        bytes.extend_from_slice(message.as_slice());
        Self { bytes }
    }

    /// A buffer [`crate::Aez::open`] has just deciphered and authenticated, `|M| + τ` bytes long:
    /// its message is the prefix before the slot.
    pub(crate) fn from_opened(mut bytes: Zeroizing<Vec<u8>>) -> Self {
        let message = bytes.len() - TAU;
        bytes.truncate(message);
        Self { bytes }
    }

    /// Return `M`.
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// `M`, still wiped on drop.
    pub fn into_zeroizing(self) -> Zeroizing<Vec<u8>> {
        self.bytes
    }

    /// `M ‖ 0^τ` for [`crate::Aez::seal`], within the reserved capacity, so `M` is not copied.
    pub(crate) fn into_slotted(mut self) -> Vec<u8> {
        let width = self.bytes.len() + TAU;
        self.bytes.resize(width, 0);
        core::mem::take(&mut *self.bytes)
    }
}
