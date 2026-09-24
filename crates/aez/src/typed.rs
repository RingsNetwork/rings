//! Owned AEZ buffers with the length precondition and the plaintext/ciphertext distinction as
//! types: `Plaintext<τ> → Ciphertext<τ>` by [`crate::Aez::seal`], and back by
//! [`crate::Aez::open`].
//!
//! ```text
//! Plaintext<τ>  ≅ { M : a buffer with spare capacity for τ more bytes }   wiped on drop, opaque
//! Ciphertext<τ> ≅ { (C, |M|) : |C| = |M| + τ }                            public data
//! seal : Plaintext<τ> → Ciphertext<τ>             total
//! open : Ciphertext<τ> → Plaintext<τ> + Inauthentic
//! open ∘ seal = Right                             (on equal key and tweak)
//! ```
//!
//! A ciphertext cannot be read as a message: only `open` produces a [`Plaintext`]. A plaintext
//! has no `Clone`, `Debug` or equality, and its buffer is zeroized when dropped. A ciphertext
//! records its message width `|M|`, established once by a checked subtraction where it is
//! accepted, so no later step computes `|C| − τ` again.

use core::iter;

use zeroize::Zeroizing;

use crate::error::ExpansionExceedsBuffer;

/// An AEZ ciphertext `C` with its message width `|M| = |C| − τ`.
///
/// Invariant: `|bytes| = message + TAU`, established by [`Ciphertext::new`] and by
/// [`crate::Aez::seal`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ciphertext<const TAU: usize> {
    /// `C`.
    bytes: Vec<u8>,
    /// `|M|`.
    message: usize,
}

/// An AEZ plaintext `M` whose buffer has spare capacity for its `τ`-byte authenticator slot;
/// zeroized on drop.
///
/// Invariant: the spare capacity of `bytes` is at least `TAU`, so appending the slot never
/// reallocates.
pub struct Plaintext<const TAU: usize> {
    /// `M`.
    bytes: Zeroizing<Vec<u8>>,
}

impl<const TAU: usize> Ciphertext<TAU> {
    /// Accept a received ciphertext.
    ///
    /// # Errors
    ///
    /// [`ExpansionExceedsBuffer`] if `|bytes| < τ`: no message is encrypted in it.
    pub fn new(bytes: Vec<u8>) -> Result<Self, ExpansionExceedsBuffer> {
        match bytes.len().checked_sub(TAU) {
            Some(message) => Ok(Self { bytes, message }),
            None => Err(ExpansionExceedsBuffer {
                length: bytes.len(),
                expansion: TAU,
            }),
        }
    }

    /// A buffer [`crate::Aez::seal`] has just enciphered: `message` bytes of `M` and the slot.
    pub(crate) const fn from_sealed(bytes: Vec<u8>, message: usize) -> Self {
        Self { bytes, message }
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

    /// `(C, |M|)` for in-place decryption, `C` wiped if it is dropped before release.
    pub(crate) fn into_parts(self) -> (Zeroizing<Vec<u8>>, usize) {
        (Zeroizing::new(self.bytes), self.message)
    }
}

impl<const TAU: usize> Plaintext<TAU> {
    /// Take a message. A buffer that already has spare capacity for the slot is kept as it is;
    /// any other is copied once into one that has, and the caller's buffer is wiped.
    pub fn new(mut message: Vec<u8>) -> Self {
        if message.spare_capacity_mut().len() >= TAU {
            return Self {
                bytes: Zeroizing::new(message),
            };
        }
        let message = Zeroizing::new(message);
        let mut bytes = message
            .iter()
            .copied()
            .chain(iter::repeat_n(0, TAU))
            .collect::<Vec<_>>();
        bytes.truncate(message.len());
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    /// A buffer [`crate::Aez::open`] has just deciphered and authenticated: its message is the
    /// first `message` bytes, and the slot it drops leaves the spare capacity of the invariant.
    pub(crate) fn from_opened(mut bytes: Zeroizing<Vec<u8>>, message: usize) -> Self {
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

    /// `(M ‖ 0^τ, |M|)` for [`crate::Aez::seal`], within the reserved capacity, so `M` is neither
    /// copied nor reallocated.
    pub(crate) fn into_slotted(mut self) -> (Vec<u8>, usize) {
        let message = self.bytes.len();
        self.bytes.extend(iter::repeat_n(0, TAU));
        (core::mem::take(&mut *self.bytes), message)
    }
}
