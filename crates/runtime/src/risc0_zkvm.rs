#![warn(missing_docs)]
//! RISC Zero backed RISC-V zkVM guest runtime.
//!
//! The RISC-V guest ABI is byte-oriented:
//!
//! ```text
//! private input  = risc0-serde(Vec<u8>) where Vec<u8> is GuestStepInput::encode_abi()
//! public journal = risc0-serde(Risc0JournalClaim {
//!     public_input: GuestPublicInput,
//!     output_abi: Vec<u8> where Vec<u8> is GuestStepOutput::encode_abi(),
//! })
//! ```
//!
//! The manifest module hash is the RISC Zero ImageID of the ELF. RISC Zero verifies that
//! a receipt proves successful execution of exactly that image and authenticates the
//! journal bytes.

use ::risc0_zkvm::compute_image_id;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use ::risc0_zkvm::get_prover_server;
use ::risc0_zkvm::Digest;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use ::risc0_zkvm::ExecutorEnv;
use ::risc0_zkvm::Journal;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use ::risc0_zkvm::ProverOpts;
use ::risc0_zkvm::Receipt as Risc0Receipt;
#[cfg(any(test, all(feature = "risc0-prove", not(target_arch = "wasm32"))))]
use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestBinary;
use super::GuestError;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestManifest;
use super::GuestProgramHash;
use super::GuestPublicInput;
use super::GuestReceipt;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestReceipt as HostGuestReceipt;
use super::GuestReceiptClaim;
use super::GuestReceiptVerifier;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestRuntime;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestRuntimeFnAdapter;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestRuntimeKind;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestRuntimeProfile;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::GuestStepInput;
use super::GuestStepOutput;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
use super::ProofPolicy;

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
const WASM_PAGE_BYTES: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Risc0JournalClaim {
    public_input: GuestPublicInput,
    output_abi: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Risc0ProvenJournal {
    public_input: GuestPublicInput,
    output: GuestStepOutput,
    output_abi_len: usize,
}

/// Compute the RISC Zero ImageID hash for an ELF guest binary.
pub fn risc0_image_id_hash(elf: &[u8]) -> Result<GuestProgramHash, GuestError> {
    let image_id = compute_image_id(elf).map_err(|error| GuestError::ProofProgramInvalid {
        reason: format!("invalid RISC Zero ELF image: {error}"),
    })?;
    GuestProgramHash::new(digest_bytes(image_id)?, "risc0_image_id")
}

/// Verifier for RISC Zero RISC-V zkVM receipts.
#[derive(Clone, Copy, Debug)]
pub struct Risc0ReceiptVerifier {
    program_hash: GuestProgramHash,
    image_id: Digest,
}

impl Risc0ReceiptVerifier {
    /// Build a verifier for the manifest module hash, interpreted as a RISC Zero ImageID.
    pub fn new(program_hash: GuestProgramHash) -> Self {
        Self {
            program_hash,
            image_id: image_id_from_program_hash(program_hash),
        }
    }
}

impl GuestReceiptVerifier for Risc0ReceiptVerifier {
    fn verify(&self, claim: &GuestReceiptClaim, receipt: &GuestReceipt) -> Result<(), GuestError> {
        if claim.program_hash != self.program_hash || !claim.matches_receipt(receipt) {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        let risc0_receipt = decode_receipt(receipt.proof.as_ref())?;
        verify_receipt_image(&risc0_receipt, self.image_id)?;
        let proven_journal = decode_proven_journal(&risc0_receipt.journal)?;
        if !proven_journal_matches_claim(&proven_journal, claim) {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        Ok(())
    }
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
/// Runtime adapter type for [`Risc0ZkvmRuntime`].
pub type Risc0ZkvmRuntimeAdapter = GuestRuntimeFnAdapter<
    fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
>;

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
/// Build the profile-tagged RISC Zero RISC-V runtime adapter.
pub fn risc0_zkvm_runtime_adapter() -> Risc0ZkvmRuntimeAdapter {
    GuestRuntimeFnAdapter::new(
        GuestRuntimeProfile::new(GuestRuntimeKind::Riscv, ProofPolicy::VerifyReceipt),
        instantiate_risc0_zkvm_runtime
            as fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
    )
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
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
        elf: binary.bytes().clone(),
        image_id: image_id_from_program_hash(image_hash),
    }))
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
/// RISC Zero backed RISC-V guest runtime.
pub struct Risc0ZkvmRuntime {
    manifest: GuestManifest,
    elf: Bytes,
    image_id: Digest,
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
impl GuestRuntime for Risc0ZkvmRuntime {
    fn step(&self, input: GuestStepInput) -> Result<GuestStepOutput, GuestError> {
        reject_dev_mode_env()?;
        let input_abi = input.encode_abi()?;
        let env = ExecutorEnv::builder()
            .write(&input_abi)
            .map_err(proof_data_encode)?
            .build()
            .map_err(proof_generation)?;
        let opts = ProverOpts::default();
        let prove_info = get_prover_server(&opts)
            .map_err(proof_generation)?
            .prove(env, self.elf.as_ref())
            .map_err(proof_generation)?;
        verify_receipt_image(&prove_info.receipt, self.image_id)?;
        let proven_journal = decode_proven_journal(&prove_info.receipt.journal)?;
        if proven_journal.public_input != input.public_input {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        let mut output = proven_journal.output;
        output.fuel_used = prove_info.stats.user_cycles;
        output.memory_pages_used = memory_pages_used(
            self.elf.len(),
            input_abi.len(),
            proven_journal.output_abi_len,
        );
        output.receipt = Some(HostGuestReceipt {
            program_hash: self.manifest.module_hash(),
            public_input: input.public_input,
            public_output: output.public_output.clone(),
            proof: encode_receipt(&prove_info.receipt)?,
        });
        Ok(output)
    }
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
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

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
fn reject_dev_mode_env() -> Result<(), GuestError> {
    if std::env::var_os("RISC0_DEV_MODE").is_some() {
        return Err(GuestError::ProofGenerationFailed {
            reason: "RISC0_DEV_MODE is not accepted for guest RISC-V proofs".to_string(),
        });
    }
    Ok(())
}

fn image_id_from_program_hash(program_hash: GuestProgramHash) -> Digest {
    Digest::from(*program_hash.as_bytes())
}

fn digest_bytes(digest: Digest) -> Result<[u8; 32], GuestError> {
    <[u8; 32]>::try_from(digest.as_bytes()).map_err(|error| GuestError::ProofProgramInvalid {
        reason: format!("RISC Zero ImageID has invalid length: {error}"),
    })
}

fn decode_receipt(bytes: &[u8]) -> Result<Risc0Receipt, GuestError> {
    rmp_serde::from_slice(bytes).map_err(|error| GuestError::ProofDataDecode {
        reason: format!("RISC Zero receipt decode failed: {error}"),
    })
}

fn verify_receipt_image(receipt: &Risc0Receipt, image_id: Digest) -> Result<(), GuestError> {
    receipt
        .verify(image_id)
        .map_err(|error| GuestError::ReceiptVerificationFailed {
            reason: format!("RISC Zero receipt verification failed: {error}"),
        })
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
fn encode_receipt(receipt: &Risc0Receipt) -> Result<Bytes, GuestError> {
    rmp_serde::to_vec(receipt)
        .map(Bytes::from)
        .map_err(|error| GuestError::ProofDataEncode {
            reason: format!("RISC Zero receipt encode failed: {error}"),
        })
}

fn decode_proven_journal(journal: &Journal) -> Result<Risc0ProvenJournal, GuestError> {
    let claim = decode_journal_claim(journal)?;
    let output = GuestStepOutput::decode_abi(claim.output_abi.as_slice())?;
    Ok(Risc0ProvenJournal {
        public_input: claim.public_input,
        output,
        output_abi_len: claim.output_abi.len(),
    })
}

fn decode_journal_claim(journal: &Journal) -> Result<Risc0JournalClaim, GuestError> {
    let words = journal_words(journal.as_ref())?;
    ::risc0_zkvm::serde::from_slice(words.as_slice()).map_err(|error| GuestError::ProofDataDecode {
        reason: format!("RISC Zero journal decode failed: {error}"),
    })
}

fn proven_journal_matches_claim(
    proven_journal: &Risc0ProvenJournal,
    claim: &GuestReceiptClaim,
) -> bool {
    proven_journal.public_input == claim.public_input
        && proven_journal.output.public_output == claim.public_output
}

fn journal_words(bytes: &[u8]) -> Result<Vec<u32>, GuestError> {
    let chunks = bytes.chunks_exact(4);
    if !chunks.remainder().is_empty() {
        return Err(GuestError::ProofDataDecode {
            reason: "RISC Zero journal length is not word-aligned".to_string(),
        });
    }
    chunks
        .map(|chunk| {
            let mut word = [0u8; 4];
            word.copy_from_slice(chunk);
            Ok(u32::from_le_bytes(word))
        })
        .collect()
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
fn memory_pages_used(elf_len: usize, input_len: usize, output_len: usize) -> u32 {
    let bytes = elf_len.saturating_add(input_len).saturating_add(output_len);
    let pages = bytes.div_ceil(WASM_PAGE_BYTES).max(1);
    u32::try_from(pages).unwrap_or(u32::MAX)
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
fn proof_generation(error: impl core::fmt::Debug) -> GuestError {
    GuestError::ProofGenerationFailed {
        reason: format!("{error:?}"),
    }
}

#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
fn proof_data_encode(error: impl core::fmt::Debug) -> GuestError {
    GuestError::ProofDataEncode {
        reason: format!("{error:?}"),
    }
}

#[cfg(test)]
mod tests {
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

    fn output() -> GuestStepOutput {
        GuestStepOutput {
            state: crate::GuestState::new(Bytes::from_static(b"next")),
            effects: Vec::new(),
            public_output: GuestPublicOutput::new(Bytes::from_static(b"out")),
            receipt: None,
            fuel_used: 1,
            memory_pages_used: 1,
        }
    }

    fn journal_bytes_for_claim(
        public_input: GuestPublicInput,
        output_abi: Vec<u8>,
    ) -> Result<Vec<u8>, GuestError> {
        let claim = Risc0JournalClaim {
            public_input,
            output_abi,
        };
        Ok(::risc0_zkvm::serde::to_vec(&claim)
            .map_err(|error| GuestError::ProofDataEncode {
                reason: format!("{error:?}"),
            })?
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>())
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

    #[test]
    fn journal_decode_rejects_non_word_aligned_bytes() {
        let journal = Journal::new(vec![1, 2, 3]);

        assert!(matches!(
            decode_journal_claim(&journal),
            Err(GuestError::ProofDataDecode { .. })
        ));
    }

    #[test]
    fn journal_decode_reads_public_input_and_guest_step_output_abi() -> Result<(), GuestError> {
        let output = output();
        let public_input = GuestPublicInput::new(Bytes::from_static(b"public-input"));
        let output_abi = output.encode_abi()?;
        let output_abi_len = output_abi.len();
        let journal = Journal::new(journal_bytes_for_claim(public_input.clone(), output_abi)?);
        let proven_journal = decode_proven_journal(&journal)?;

        assert_eq!(proven_journal.public_input, public_input);
        assert_eq!(proven_journal.output, output);
        assert_eq!(proven_journal.output_abi_len, output_abi_len);
        Ok(())
    }

    #[test]
    fn proven_journal_match_requires_public_input_and_public_output() -> Result<(), GuestError> {
        let output = output();
        let proven_journal = Risc0ProvenJournal {
            public_input: GuestPublicInput::new(Bytes::from_static(b"in")),
            output: output.clone(),
            output_abi_len: 0,
        };
        let matching_claim = GuestReceiptClaim::new(
            hash(3)?,
            GuestPublicInput::new(Bytes::from_static(b"in")),
            output.public_output.clone(),
        );
        let mismatched_input = GuestReceiptClaim::new(
            hash(3)?,
            GuestPublicInput::new(Bytes::from_static(b"other")),
            output.public_output.clone(),
        );
        let mismatched_output = GuestReceiptClaim::new(
            hash(3)?,
            GuestPublicInput::new(Bytes::from_static(b"in")),
            GuestPublicOutput::new(Bytes::from_static(b"other")),
        );

        assert!(proven_journal_matches_claim(
            &proven_journal,
            &matching_claim
        ));
        assert!(!proven_journal_matches_claim(
            &proven_journal,
            &mismatched_input
        ));
        assert!(!proven_journal_matches_claim(
            &proven_journal,
            &mismatched_output
        ));
        Ok(())
    }

    #[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
    #[test]
    fn memory_pages_used_rounds_up_to_wasm_pages() {
        assert_eq!(memory_pages_used(1, 1, 1), 1);
        assert_eq!(memory_pages_used(WASM_PAGE_BYTES, 1, 0), 2);
    }

    #[cfg(target_arch = "wasm32")]
    mod wasm32 {
        use wasm_bindgen_test::wasm_bindgen_test;
        use wasm_bindgen_test::wasm_bindgen_test_configure;

        use super::*;

        wasm_bindgen_test_configure!(run_in_browser);

        #[wasm_bindgen_test]
        fn risc0_verifier_wasm32_rejects_malformed_receipt() {
            let claim = match claim() {
                Ok(claim) => claim,
                Err(error) => {
                    assert!(false, "valid test claim failed: {error}");
                    return;
                }
            };

            assert!(matches!(
                Risc0ReceiptVerifier::new(claim.program_hash)
                    .verify(&claim, &receipt(&claim, Bytes::from_static(b"not msgpack"))),
                Err(GuestError::ProofDataDecode { .. })
            ));
        }
    }
}
