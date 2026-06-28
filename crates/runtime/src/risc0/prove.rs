//! Native RISC Zero proving adapter.

use super::proof_error;
use super::risc0_image_id_hash;
use crate::GuestBinary;
use crate::GuestError;
use crate::GuestManifest;
use crate::GuestReceipt;
use crate::GuestRuntime;
use crate::GuestRuntimeFnAdapter;
use crate::GuestRuntimeKind;
use crate::GuestRuntimeProfile;
use crate::GuestStepInput;
use crate::GuestStepOutput;
use crate::ProofPolicy;

const WASM_PAGE_BYTES: usize = 65_536;

/// Runtime adapter type for [`Risc0ZkvmRuntime`].
pub type Risc0ZkvmRuntimeAdapter = GuestRuntimeFnAdapter<
    fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
>;

/// Build the profile-tagged RISC Zero RISC-V runtime adapter.
pub fn risc0_zkvm_runtime_adapter() -> Risc0ZkvmRuntimeAdapter {
    GuestRuntimeFnAdapter::new(
        GuestRuntimeProfile::new(GuestRuntimeKind::Riscv, ProofPolicy::VerifyReceipt),
        instantiate_risc0_zkvm_runtime
            as fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
    )
}

/// Instantiate a RISC Zero RISC-V runtime from a manifest and ELF guest binary.
pub fn instantiate_risc0_zkvm_runtime(
    manifest: &GuestManifest,
    binary: GuestBinary,
) -> Result<Box<dyn GuestRuntime>, GuestError> {
    require_risc0_profile(manifest)?;
    binary.validate_manifest(manifest)?;
    let image_hash = risc0_image_id_hash(binary.bytes().as_ref())?;
    if image_hash != manifest.module_hash() {
        return Err(GuestError::ProgramHashMismatch {
            expected: manifest.module_hash(),
            actual: image_hash,
        });
    }
    Ok(Box::new(Risc0ZkvmRuntime {
        manifest: manifest.clone(),
        elf_len: binary.bytes().len(),
        prover: rings_proofs::risc0::Risc0ZkvmProver::new(
            binary.bytes().clone(),
            *image_hash.as_bytes(),
        ),
    }))
}

/// RISC Zero backed RISC-V guest runtime.
pub struct Risc0ZkvmRuntime {
    manifest: GuestManifest,
    elf_len: usize,
    prover: rings_proofs::risc0::Risc0ZkvmProver,
}

impl GuestRuntime for Risc0ZkvmRuntime {
    fn step(&self, input: GuestStepInput) -> Result<GuestStepOutput, GuestError> {
        let input_abi = input.encode_abi()?;
        let proof = self
            .prover
            .prove(input_abi.as_slice())
            .map_err(proof_error)?;
        if proof.public_input != input.public_input.bytes().clone() {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        let output_abi_len = proof.output_abi.len();
        let mut output = GuestStepOutput::decode_abi(proof.output_abi.as_slice())?;
        output.fuel_used = proof.user_cycles;
        output.memory_pages_used = memory_pages_used(self.elf_len, input_abi.len(), output_abi_len);
        output.receipt = Some(GuestReceipt {
            program_hash: self.manifest.module_hash(),
            public_input: input.public_input,
            public_output: output.public_output.clone(),
            proof: proof.proof,
        });
        Ok(output)
    }
}

fn require_risc0_profile(manifest: &GuestManifest) -> Result<(), GuestError> {
    if manifest.runtime() == GuestRuntimeKind::Riscv
        && manifest.proof_policy() == ProofPolicy::VerifyReceipt
    {
        return Ok(());
    }
    Err(GuestError::RuntimeRejected {
        reason: "RISC Zero adapter requires riscv + verify_receipt".to_string(),
    })
}

fn memory_pages_used(elf_len: usize, input_len: usize, output_len: usize) -> u32 {
    let bytes = elf_len.saturating_add(input_len).saturating_add(output_len);
    let pages = bytes.div_ceil(WASM_PAGE_BYTES).max(1);
    u32::try_from(pages).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_pages_used_rounds_up_to_wasm_pages() {
        assert_eq!(memory_pages_used(1, 1, 1), 1);
        assert_eq!(memory_pages_used(WASM_PAGE_BYTES, 1, 0), 2);
    }
}
