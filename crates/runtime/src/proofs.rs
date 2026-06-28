//! Runtime boundary helpers for proof adapters.

use rings_proofs::ProofClaim;
use rings_proofs::ProofError;

use super::GuestError;
use super::GuestReceiptClaim;

pub(crate) fn proof_claim_from_guest(claim: &GuestReceiptClaim) -> ProofClaim {
    ProofClaim::new(
        *claim.program_hash.as_bytes(),
        claim.public_input.bytes().clone(),
        claim.public_output.bytes().clone(),
    )
}

pub(crate) fn proof_error(error: ProofError) -> GuestError {
    match error {
        ProofError::ProgramInvalid { reason } => GuestError::ProofProgramInvalid { reason },
        ProofError::DataDecode { reason } => GuestError::ProofDataDecode { reason },
        ProofError::DataEncode { reason } => GuestError::ProofDataEncode { reason },
        ProofError::GenerationFailed { reason } => GuestError::ProofGenerationFailed { reason },
        ProofError::VerificationFailed { reason } => {
            GuestError::ReceiptVerificationFailed { reason }
        }
        ProofError::ClaimMismatch => GuestError::ReceiptClaimMismatch,
    }
}
