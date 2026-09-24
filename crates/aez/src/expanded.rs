//! A byte buffer `M ‖ A` whose authenticator slot `A` has `τ` bytes: the length precondition of
//! `Encrypt` and `Decrypt` as a type.
//!
//! ```text
//! Expanded<τ> ≅ { w ∈ {0,1}^{8n} : n ≥ τ }
//! with_authenticator_slot : M ↦ M ‖ 0^τ                total
//! new                     : w ⇀ w                       defined iff |w| ≥ τ
//! into_message            : M ‖ A ↦ M                   total
//! ```
//!
//! [`crate::Aez::encrypt_expanded`] and [`crate::Aez::decrypt_expanded`] take this type, so the
//! length is checked once, where a buffer enters, and not at every call.

use crate::error::ExpansionExceedsBuffer;

/// A buffer of at least `TAU` bytes, whose last `TAU` bytes are the authenticator slot.
///
/// Invariant: `|bytes| ≥ TAU`, established by both constructors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Expanded<const TAU: usize> {
    /// `M ‖ A`.
    bytes: Vec<u8>,
}

impl<const TAU: usize> Expanded<TAU> {
    /// Accept a received buffer.
    ///
    /// # Errors
    ///
    /// [`ExpansionExceedsBuffer`] if `|bytes| < τ`.
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

    /// `M ↦ M ‖ 0^τ`: a message with an empty authenticator slot appended.
    pub fn with_authenticator_slot(mut message: Vec<u8>) -> Self {
        message.extend([0; TAU]);
        Self { bytes: message }
    }

    /// Return `M ‖ A`.
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Return `M ‖ A` for an in-place, length-preserving transformation.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.bytes.as_mut_slice()
    }

    /// Return the authenticator slot `A`, the last `τ` bytes.
    pub(crate) fn authenticator_mut(&mut self) -> &mut [u8] {
        let message = self.bytes.len().saturating_sub(TAU);
        self.bytes.split_at_mut(message).1
    }

    /// `M ‖ A ↦ M`: the buffer without its authenticator slot.
    pub fn into_message(mut self) -> Vec<u8> {
        self.bytes.truncate(self.bytes.len().saturating_sub(TAU));
        self.bytes
    }
}
