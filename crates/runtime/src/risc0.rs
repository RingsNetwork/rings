//! Runtime adapter for RISC Zero backed RISC-V guest execution.

use rings_proofs::ProofSystem;

use super::proofs::proof_claim_from_guest;
use super::proofs::proof_error;
use super::GuestError;
use super::GuestProgramHash;
use super::GuestReceipt;
use super::GuestReceiptClaim;
use super::GuestReceiptVerifier;
use super::GuestStepOutput;

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
mod prove;

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use prove::instantiate_risc0_zkvm_runtime;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use prove::risc0_zkvm_runtime_adapter;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use prove::Risc0ZkvmRuntime;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use prove::Risc0ZkvmRuntimeAdapter;

/// Compute the RISC Zero ImageID hash for an ELF guest binary.
pub fn risc0_image_id_hash(elf: &[u8]) -> Result<GuestProgramHash, GuestError> {
    let image_id = rings_proofs::risc0::risc0_image_id_hash(elf).map_err(proof_error)?;
    GuestProgramHash::new(image_id, "risc0_image_id")
}

/// Verifier for RISC Zero RISC-V zkVM receipts.
#[derive(Clone, Copy, Debug)]
pub struct Risc0ReceiptVerifier {
    program_hash: GuestProgramHash,
    verifier: rings_proofs::risc0::Risc0ReceiptVerifier,
}

impl Risc0ReceiptVerifier {
    /// Build a verifier for the manifest module hash, interpreted as a RISC Zero ImageID.
    pub fn new(program_hash: GuestProgramHash) -> Self {
        Self {
            program_hash,
            verifier: rings_proofs::risc0::Risc0ReceiptVerifier::new(*program_hash.as_bytes()),
        }
    }
}

impl GuestReceiptVerifier for Risc0ReceiptVerifier {
    fn verify(&self, claim: &GuestReceiptClaim, receipt: &GuestReceipt) -> Result<(), GuestError> {
        if claim.program_hash != self.program_hash || !claim.matches_receipt(receipt) {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        let verified = self
            .verifier
            .verify(&proof_claim_from_guest(claim), receipt.proof.as_ref())
            .map_err(proof_error)?;
        let output = GuestStepOutput::decode_abi(verified.output_abi())?;
        if output.public_output != claim.public_output {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::GuestPublicInput;
    use crate::GuestPublicOutput;

    fn hash(seed: u8) -> Result<GuestProgramHash, GuestError> {
        GuestProgramHash::new([seed; 32], "test")
    }

    fn claim() -> Result<GuestReceiptClaim, GuestError> {
        Ok(GuestReceiptClaim::new(
            hash(3)?,
            GuestPublicInput::new(Bytes::from_static(b"in")),
            GuestPublicOutput::new(Bytes::from_static(b"out")),
        ))
    }

    fn receipt(claim: &GuestReceiptClaim, proof: impl Into<Bytes>) -> GuestReceipt {
        GuestReceipt {
            program_hash: claim.program_hash,
            public_input: claim.public_input.clone(),
            public_output: claim.public_output.clone(),
            proof: proof.into(),
        }
    }

    #[test]
    fn risc0_image_id_rejects_malformed_elf() {
        assert!(matches!(
            risc0_image_id_hash(b"not an elf"),
            Err(GuestError::ProofProgramInvalid { .. })
        ));
    }

    #[test]
    fn verifier_rejects_claim_mismatch_before_receipt_decode() -> Result<(), GuestError> {
        let claim = claim()?;
        let other_claim = GuestReceiptClaim::new(
            hash(9)?,
            claim.public_input.clone(),
            claim.public_output.clone(),
        );

        assert_eq!(
            Risc0ReceiptVerifier::new(claim.program_hash).verify(
                &other_claim,
                &receipt(&claim, Bytes::from_static(b"not msgpack")),
            ),
            Err(GuestError::ReceiptClaimMismatch)
        );
        Ok(())
    }

    #[test]
    fn verifier_rejects_malformed_receipt_after_claim_match() -> Result<(), GuestError> {
        let claim = claim()?;

        assert!(matches!(
            Risc0ReceiptVerifier::new(claim.program_hash)
                .verify(&claim, &receipt(&claim, Bytes::from_static(b"not msgpack"))),
            Err(GuestError::ProofDataDecode { .. })
        ));
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
            let claim_result = claim();
            assert!(claim_result.is_ok());
            let Some(claim) = claim_result.ok() else {
                return;
            };

            assert!(matches!(
                Risc0ReceiptVerifier::new(claim.program_hash)
                    .verify(&claim, &receipt(&claim, Bytes::from_static(b"not msgpack"))),
                Err(GuestError::ProofDataDecode { .. })
            ));
        }
    }
}
