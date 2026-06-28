//! RISC Zero proof adapter for RISC-V guest receipts.
//!
//! The RISC-V guest ABI is byte-oriented:
//!
//! ```text
//! private input  = risc0-serde(Vec<u8>)
//! public journal = risc0-serde(Risc0JournalClaim {
//!     public_input: Vec<u8>,
//!     output_abi: Vec<u8>,
//! })
//! ```
//!
//! The runtime layer owns the guest ABI. This module verifies the RISC Zero
//! receipt and returns the authenticated output ABI bytes to that layer.

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
mod prove;

use bytes::Bytes;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use prove::Risc0ProveOutput;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use prove::Risc0ZkvmProver;
use risc0_zkvm::compute_image_id;
use risc0_zkvm::Digest;
use risc0_zkvm::Journal;
use risc0_zkvm::Receipt as Risc0Receipt;
use serde::Deserialize;
use serde::Serialize;

use crate::data_decode;
use crate::ProofClaim;
use crate::ProofError;
use crate::ProofSystem;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Risc0JournalClaim {
    public_input: Bytes,
    output_abi: Vec<u8>,
}

/// Authenticated journal bytes produced by a verified RISC Zero receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRisc0Receipt {
    public_input: Bytes,
    output_abi: Vec<u8>,
}

impl VerifiedRisc0Receipt {
    /// Public input committed in the verified journal.
    pub fn public_input(&self) -> &Bytes {
        &self.public_input
    }

    /// Guest step output ABI committed in the verified journal.
    pub fn output_abi(&self) -> &[u8] {
        self.output_abi.as_slice()
    }

    /// Length of the guest step output ABI.
    pub fn output_abi_len(&self) -> usize {
        self.output_abi.len()
    }
}

/// Verifier for RISC Zero RISC-V zkVM receipts.
#[derive(Clone, Copy, Debug)]
pub struct Risc0ReceiptVerifier {
    program_hash: [u8; 32],
    image_id: Digest,
}

impl Risc0ReceiptVerifier {
    /// Build a verifier for the manifest module hash, interpreted as a RISC Zero ImageID.
    pub fn new(program_hash: [u8; 32]) -> Self {
        Self {
            program_hash,
            image_id: image_id_from_program_hash(program_hash),
        }
    }
}

impl ProofSystem for Risc0ReceiptVerifier {
    type Verified = VerifiedRisc0Receipt;

    fn verify(&self, claim: &ProofClaim, proof: &[u8]) -> Result<Self::Verified, ProofError> {
        if claim.program_hash() != &self.program_hash {
            return Err(ProofError::ClaimMismatch);
        }
        let receipt = decode_receipt(proof)?;
        verify_receipt_image(&receipt, self.image_id)?;
        let verified = decode_verified_journal(&receipt.journal)?;
        if verified.public_input() != claim.public_input() {
            return Err(ProofError::ClaimMismatch);
        }
        Ok(verified)
    }
}

/// Compute the RISC Zero ImageID hash for an ELF guest binary.
pub fn risc0_image_id_hash(elf: &[u8]) -> Result<[u8; 32], ProofError> {
    let image_id = compute_image_id(elf).map_err(|error| ProofError::ProgramInvalid {
        reason: format!("invalid RISC Zero ELF image: {error}"),
    })?;
    digest_bytes(image_id)
}

pub(crate) fn image_id_from_program_hash(program_hash: [u8; 32]) -> Digest {
    Digest::from(program_hash)
}

pub(crate) fn digest_bytes(digest: Digest) -> Result<[u8; 32], ProofError> {
    <[u8; 32]>::try_from(digest.as_bytes()).map_err(|error| ProofError::ProgramInvalid {
        reason: format!("RISC Zero ImageID has invalid length: {error}"),
    })
}

pub(crate) fn decode_receipt(bytes: &[u8]) -> Result<Risc0Receipt, ProofError> {
    rmp_serde::from_slice(bytes)
        .map_err(|error| data_decode(format!("RISC Zero receipt decode failed: {error}")))
}

pub(crate) fn verify_receipt_image(
    receipt: &Risc0Receipt,
    image_id: Digest,
) -> Result<(), ProofError> {
    receipt
        .verify(image_id)
        .map_err(|error| ProofError::VerificationFailed {
            reason: format!("RISC Zero receipt verification failed: {error}"),
        })
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub(crate) fn encode_receipt(receipt: &Risc0Receipt) -> Result<Bytes, ProofError> {
    rmp_serde::to_vec(receipt)
        .map(Bytes::from)
        .map_err(|error| crate::data_encode(format!("RISC Zero receipt encode failed: {error}")))
}

pub(crate) fn decode_verified_journal(
    journal: &Journal,
) -> Result<VerifiedRisc0Receipt, ProofError> {
    let claim = decode_journal_claim(journal)?;
    Ok(VerifiedRisc0Receipt {
        public_input: claim.public_input,
        output_abi: claim.output_abi,
    })
}

fn decode_journal_claim(journal: &Journal) -> Result<Risc0JournalClaim, ProofError> {
    let words = journal_words(journal.as_ref())?;
    risc0_zkvm::serde::from_slice(words.as_slice())
        .map_err(|error| data_decode(format!("RISC Zero journal decode failed: {error}")))
}

fn journal_words(bytes: &[u8]) -> Result<Vec<u32>, ProofError> {
    let chunks = bytes.chunks_exact(4);
    if !chunks.remainder().is_empty() {
        return Err(data_decode("RISC Zero journal length is not word-aligned"));
    }
    chunks
        .map(|chunk| {
            let mut word = [0u8; 4];
            word.copy_from_slice(chunk);
            Ok(u32::from_le_bytes(word))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn claim() -> ProofClaim {
        ProofClaim::new(
            hash(3),
            Bytes::from_static(b"in"),
            Bytes::from_static(b"out"),
        )
    }

    fn journal_bytes_for_claim(
        public_input: Bytes,
        output_abi: Vec<u8>,
    ) -> Result<Vec<u8>, ProofError> {
        let claim = Risc0JournalClaim {
            public_input,
            output_abi,
        };
        Ok(risc0_zkvm::serde::to_vec(&claim)
            .map_err(|error| crate::data_encode(format!("{error:?}")))?
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>())
    }

    #[test]
    fn risc0_image_id_rejects_malformed_elf() {
        assert!(matches!(
            risc0_image_id_hash(b"not an elf"),
            Err(ProofError::ProgramInvalid { .. })
        ));
    }

    #[test]
    fn verifier_rejects_claim_mismatch_before_receipt_decode() {
        assert_eq!(
            Risc0ReceiptVerifier::new(hash(3))
                .verify(
                    &ProofClaim::new(hash(9), Bytes::new(), Bytes::new()),
                    b"not msgpack",
                )
                .map(|_| ()),
            Err(ProofError::ClaimMismatch)
        );
    }

    #[test]
    fn verifier_rejects_malformed_receipt_after_claim_match() {
        assert!(matches!(
            Risc0ReceiptVerifier::new(hash(3)).verify(&claim(), b"not msgpack"),
            Err(ProofError::DataDecode { .. })
        ));
    }

    #[test]
    fn journal_decode_rejects_non_word_aligned_bytes() {
        let journal = Journal::new(vec![1, 2, 3]);

        assert!(matches!(
            decode_journal_claim(&journal),
            Err(ProofError::DataDecode { .. })
        ));
    }

    #[test]
    fn journal_decode_reads_public_input_and_output_abi() -> Result<(), ProofError> {
        let public_input = Bytes::from_static(b"public-input");
        let output_abi = b"guest-output-abi".to_vec();
        let journal = Journal::new(journal_bytes_for_claim(
            public_input.clone(),
            output_abi.clone(),
        )?);
        let verified = decode_verified_journal(&journal)?;

        assert_eq!(verified.public_input(), &public_input);
        assert_eq!(verified.output_abi(), output_abi.as_slice());
        assert_eq!(verified.output_abi_len(), output_abi.len());
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    mod wasm32 {
        use wasm_bindgen_test::wasm_bindgen_test;
        use wasm_bindgen_test::wasm_bindgen_test_configure;

        use super::*;

        wasm_bindgen_test_configure!(run_in_browser);

        #[wasm_bindgen_test]
        fn risc0_verifier_wasm32_rejects_malformed_receipt() {
            assert!(matches!(
                Risc0ReceiptVerifier::new(hash(3)).verify(&claim(), b"not msgpack"),
                Err(ProofError::DataDecode { .. })
            ));
        }
    }
}
