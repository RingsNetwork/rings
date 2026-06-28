//! Native RISC Zero prover support.

use bytes::Bytes;
use risc0_zkvm::get_prover_server;
use risc0_zkvm::ExecutorEnv;
use risc0_zkvm::ProverOpts;

use super::decode_verified_journal;
use super::encode_receipt;
use super::image_id_from_program_hash;
use super::verify_receipt_image;
use crate::ProofError;

/// Receipt and journal data produced by a native RISC Zero proof run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Risc0ProveOutput {
    /// Public input committed by the guest journal.
    pub public_input: Bytes,
    /// Guest step output ABI committed by the guest journal.
    pub output_abi: Vec<u8>,
    /// Encoded RISC Zero receipt.
    pub proof: Bytes,
    /// User cycles reported by RISC Zero.
    pub user_cycles: u64,
}

/// Native RISC Zero prover for one guest ELF.
pub struct Risc0ZkvmProver {
    elf: Bytes,
    image_id: risc0_zkvm::Digest,
}

impl Risc0ZkvmProver {
    /// Build a prover for an ELF and expected ImageID hash.
    pub fn new(elf: Bytes, program_hash: [u8; 32]) -> Self {
        Self {
            elf,
            image_id: image_id_from_program_hash(program_hash),
        }
    }

    /// Run the guest and produce a receipt for an encoded guest step input.
    pub fn prove(&self, input_abi: &[u8]) -> Result<Risc0ProveOutput, ProofError> {
        reject_dev_mode_env()?;
        let input_abi = input_abi.to_vec();
        let env = ExecutorEnv::builder()
            .write(&input_abi)
            .map_err(data_encode)?
            .build()
            .map_err(generation_failed)?;
        let opts = ProverOpts::default();
        let prove_info = get_prover_server(&opts)
            .map_err(generation_failed)?
            .prove(env, self.elf.as_ref())
            .map_err(generation_failed)?;
        verify_receipt_image(&prove_info.receipt, self.image_id)?;
        let verified = decode_verified_journal(&prove_info.receipt.journal)?;
        Ok(Risc0ProveOutput {
            public_input: verified.public_input().clone(),
            output_abi: verified.output_abi().to_vec(),
            proof: encode_receipt(&prove_info.receipt)?,
            user_cycles: prove_info.stats.user_cycles,
        })
    }
}

fn reject_dev_mode_env() -> Result<(), ProofError> {
    if std::env::var_os("RISC0_DEV_MODE").is_some() {
        return Err(ProofError::GenerationFailed {
            reason: "RISC0_DEV_MODE is not accepted for guest RISC-V proofs".to_string(),
        });
    }
    Ok(())
}

fn generation_failed(error: impl core::fmt::Debug) -> ProofError {
    ProofError::GenerationFailed {
        reason: format!("{error:?}"),
    }
}

fn data_encode(error: impl core::fmt::Debug) -> ProofError {
    ProofError::DataEncode {
        reason: format!("{error:?}"),
    }
}
