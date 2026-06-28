//! Runtime adapter for Delphinus zkWasm aggregator receipts.

pub use rings_proofs::zkwasm::ZkWasmAggregatorReceipt;
use rings_proofs::ProofSystem;

use super::proofs::proof_claim_from_guest;
use super::proofs::proof_error;
use super::GuestError;
use super::GuestReceipt;
use super::GuestReceiptClaim;
use super::GuestReceiptVerifier;

/// Verifier for Delphinus zkWasm aggregator receipts.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZkWasmAggregatorVerifier {
    verifier: rings_proofs::zkwasm::ZkWasmAggregatorVerifier,
}

impl ZkWasmAggregatorVerifier {
    /// Build a zkWasm aggregator verifier.
    pub fn new() -> Self {
        Self {
            verifier: rings_proofs::zkwasm::ZkWasmAggregatorVerifier::new(),
        }
    }
}

impl GuestReceiptVerifier for ZkWasmAggregatorVerifier {
    fn verify(&self, claim: &GuestReceiptClaim, receipt: &GuestReceipt) -> Result<(), GuestError> {
        if !claim.matches_receipt(receipt) {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        self.verifier
            .verify(&proof_claim_from_guest(claim), receipt.proof.as_ref())
            .map_err(proof_error)
    }
}

/// Compute the first target instance that binds a zkWasm proof to a Rings receipt claim.
pub fn zkwasm_aggregator_claim_scalar(claim: &GuestReceiptClaim) -> [u8; 32] {
    rings_proofs::zkwasm::zkwasm_aggregator_claim_scalar(&proof_claim_from_guest(claim))
}
