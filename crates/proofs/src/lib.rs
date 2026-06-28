#![warn(missing_docs)]
//! Proof system adapters for guest extension receipts.
//!
//! This crate owns backend-specific proof verification and proof production.
//! Runtime crates pass byte-oriented claims into these backends and remain
//! responsible for interpreting any verified guest ABI payload.

use bytes::Bytes;

#[cfg(feature = "risc0-proof")]
pub mod risc0;
#[cfg(feature = "wasm-proof")]
pub mod zkwasm;

/// Public receipt claim bound to a proof backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProofClaim {
    program_hash: [u8; 32],
    public_input: Bytes,
    public_output: Bytes,
}

impl ProofClaim {
    /// Build a proof claim from already-canonical byte fields.
    pub fn new(
        program_hash: [u8; 32],
        public_input: impl Into<Bytes>,
        public_output: impl Into<Bytes>,
    ) -> Self {
        Self {
            program_hash,
            public_input: public_input.into(),
            public_output: public_output.into(),
        }
    }

    /// Program hash committed by the receipt envelope.
    pub fn program_hash(&self) -> &[u8; 32] {
        &self.program_hash
    }

    /// Public input committed by the receipt envelope.
    pub fn public_input(&self) -> &Bytes {
        &self.public_input
    }

    /// Public output committed by the receipt envelope.
    pub fn public_output(&self) -> &Bytes {
        &self.public_output
    }
}

/// Proof adapter error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProofError {
    /// The proving program cannot be used by the backend.
    #[error("proof program is invalid: {reason}")]
    ProgramInvalid {
        /// Validation reason.
        reason: String,
    },
    /// A proof, witness, or receipt could not be decoded.
    #[error("proof data decode failed: {reason}")]
    DataDecode {
        /// Decoding reason.
        reason: String,
    },
    /// A proof, witness, or receipt could not be encoded.
    #[error("proof data encode failed: {reason}")]
    DataEncode {
        /// Encoding reason.
        reason: String,
    },
    /// Proof generation failed.
    #[error("proof generation failed: {reason}")]
    GenerationFailed {
        /// Prover reason.
        reason: String,
    },
    /// Proof verification failed.
    #[error("proof verification failed: {reason}")]
    VerificationFailed {
        /// Verifier reason.
        reason: String,
    },
    /// The proof does not bind to the requested public claim.
    #[error("proof claim mismatch")]
    ClaimMismatch,
}

/// Proof verifier interface for guest receipt backends.
pub trait ProofSystem {
    /// Backend-specific verified payload returned to the runtime layer.
    type Verified;

    /// Verify a backend proof against a public claim.
    fn verify(&self, claim: &ProofClaim, proof: &[u8]) -> Result<Self::Verified, ProofError>;
}

/// Build a decode error.
pub(crate) fn data_decode(reason: impl Into<String>) -> ProofError {
    ProofError::DataDecode {
        reason: reason.into(),
    }
}

/// Build an encode error.
pub(crate) fn data_encode(reason: impl Into<String>) -> ProofError {
    ProofError::DataEncode {
        reason: reason.into(),
    }
}

/// Compute the Keccak-256 hash of input bytes.
#[cfg(feature = "wasm-proof")]
pub(crate) fn keccak256(bytes: &[u8]) -> [u8; 32] {
    use tiny_keccak::Hasher;
    use tiny_keccak::Keccak;

    let mut output = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(bytes);
    hasher.finalize(&mut output);
    output
}
