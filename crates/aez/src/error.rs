//! Failures of AEZ key construction, encryption and decryption.

/// One of the three subkeys of `K = I ‖ J ‖ L`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subkey {
    /// `I`, bytes `0..16` of the key.
    I,
    /// `J`, bytes `16..32` of the key.
    J,
    /// `L`, bytes `32..48` of the key.
    L,
}

impl core::fmt::Display for Subkey {
    /// Writes the subkey's name as in the AEZ specification.
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::I => "I",
            Self::J => "J",
            Self::L => "L",
        })
    }
}

/// A 384-bit key that AEZ must not be used with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The subkey is `0^128`, a weak key of the AEZ tweakable blockcipher.
    #[error("AEZ subkey {0} is zero")]
    ZeroSubkey(Subkey),
}

/// The buffer cannot hold the `τ`-byte authenticator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{expansion}-byte AEZ authenticator exceeds the {length}-byte buffer")]
pub struct ExpansionExceedsBuffer {
    /// Buffer length in bytes.
    pub length: usize,
    /// Requested expansion `τ` in bytes.
    pub expansion: usize,
}

/// Why a ciphertext was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecryptError {
    /// The ciphertext is shorter than `τ`, so it encrypts no message.
    #[error(transparent)]
    Truncated(#[from] ExpansionExceedsBuffer),
    /// The deciphered authenticator is not `0^τ`; the buffer has been zeroized.
    #[error(transparent)]
    Inauthentic(#[from] Inauthentic),
}

/// The deciphered authenticator is not `0^τ`; the buffer has been zeroized.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("AEZ authenticator mismatch")]
pub struct Inauthentic;
