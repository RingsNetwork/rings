#![warn(missing_docs)]
//! Spartan-backed R1CS guest runtime.
//!
//! The R1CS guest program is a Spartan statement over the curve25519 scalar
//! field. A guest transition supplies private variables plus public outputs.
//! The runtime proves satisfiability and emits a receipt. The verifier rebuilds
//! the same public inputs from the expected receipt claim and verifies the
//! Spartan proof against the program commitment.

use std::cmp;
use std::convert::TryFrom;

use bytes::Bytes;
use curve25519_dalek::scalar::Scalar;
use libspartan::ComputationCommitment;
use libspartan::ComputationDecommitment;
use libspartan::InputsAssignment;
use libspartan::Instance;
use libspartan::SNARKGens;
use libspartan::VarsAssignment;
use libspartan::SNARK as SpartanSnark;
use merlin::Transcript;
use rings_core::ecc::keccak256;
use serde::Deserialize;
use serde::Serialize;

use super::GuestBinary;
use super::GuestError;
use super::GuestManifest;
use super::GuestPublicOutput;
use super::GuestReceipt;
use super::GuestReceiptClaim;
use super::GuestReceiptVerifier;
use super::GuestRuntime;
use super::GuestRuntimeFnAdapter;
use super::GuestRuntimeKind;
use super::GuestRuntimeProfile;
use super::GuestState;
use super::GuestStepInput;
use super::GuestStepOutput;
use super::ProofPolicy;

const CLAIM_BINDING_DOMAIN: &[u8] = b"rings:guest:r1cs:spartan:claim:v1";
const TRANSCRIPT_DOMAIN: &[u8] = b"rings_guest_r1cs_spartan_v1";
const SCALAR_BYTES: usize = 32;
const SCALAR_BYTES_U64: u64 = 32;
const WASM_PAGE_BYTES: u64 = 65_536;

/// Sparse matrix entry in a Spartan R1CS program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpartanR1csEntry {
    /// Constraint row.
    pub row: u32,
    /// Variable/input column in Spartan order: variables, one, public inputs.
    pub col: u32,
    /// Canonical curve25519 scalar bytes.
    pub value: [u8; SCALAR_BYTES],
}

/// Serializable Spartan R1CS guest program.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpartanR1csProgramSpec {
    /// Number of constraints.
    pub num_constraints: u32,
    /// Number of private variables.
    pub num_variables: u32,
    /// Number of public inputs. The first input is the receipt claim binding.
    pub num_public_inputs: u32,
    /// Sparse A matrix.
    pub a: Vec<SpartanR1csEntry>,
    /// Sparse B matrix.
    pub b: Vec<SpartanR1csEntry>,
    /// Sparse C matrix.
    pub c: Vec<SpartanR1csEntry>,
}

/// Validated Spartan R1CS guest program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpartanR1csProgram {
    spec: SpartanR1csProgramSpec,
}

/// Witness supplied as the guest event payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpartanR1csStepWitness {
    /// Private variable assignment in Spartan variable order.
    pub variables: Vec<[u8; SCALAR_BYTES]>,
    /// Public output scalars. These become public inputs after the claim binding.
    pub public_outputs: Vec<[u8; SCALAR_BYTES]>,
    /// Next opaque guest state.
    pub next_state: Bytes,
}

/// Spartan-backed R1CS runtime.
pub struct SpartanR1csRuntime {
    manifest: GuestManifest,
    program: SpartanR1csProgram,
}

/// Verifier for receipts produced by [`SpartanR1csRuntime`].
pub struct SpartanR1csVerifier {
    program_hash: super::GuestProgramHash,
    program: SpartanR1csProgram,
}

/// Function-adapter type for [`SpartanR1csRuntime`].
pub type SpartanR1csRuntimeAdapter = GuestRuntimeFnAdapter<
    fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
>;

/// Build the profile-tagged R1CS/Spartan runtime adapter.
pub fn spartan_r1cs_runtime_adapter() -> SpartanR1csRuntimeAdapter {
    GuestRuntimeFnAdapter::new(
        GuestRuntimeProfile::new(GuestRuntimeKind::R1cs, ProofPolicy::VerifyReceipt),
        instantiate_spartan_r1cs_runtime
            as fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
    )
}

/// Instantiate a Spartan R1CS runtime from a manifest and serialized program.
pub fn instantiate_spartan_r1cs_runtime(
    manifest: &GuestManifest,
    binary: GuestBinary,
) -> Result<Box<dyn GuestRuntime>, GuestError> {
    require_spartan_profile(manifest)?;
    binary.validate_manifest(manifest)?;
    let program = SpartanR1csProgram::decode(binary.bytes())?;
    Ok(Box::new(SpartanR1csRuntime {
        manifest: manifest.clone(),
        program,
    }))
}

/// Compute the claim-binding public input used by Spartan R1CS guest programs.
pub fn spartan_r1cs_claim_scalar(claim: &GuestReceiptClaim) -> [u8; SCALAR_BYTES] {
    let mut transcript = Vec::new();
    transcript.extend_from_slice(CLAIM_BINDING_DOMAIN);
    transcript.extend_from_slice(claim.program_hash.as_bytes());
    append_len_prefixed(&mut transcript, claim.public_input.bytes());
    append_len_prefixed(&mut transcript, claim.public_output.bytes());
    Scalar::from_bytes_mod_order(keccak256(transcript.as_slice())).to_bytes()
}

impl SpartanR1csProgram {
    /// Validate a serialized program spec.
    pub fn new(spec: SpartanR1csProgramSpec) -> Result<Self, GuestError> {
        if spec.num_constraints == 0 {
            return invalid_program("constraint count must be non-zero");
        }
        if spec.num_variables == 0 {
            return invalid_program("variable count must be non-zero");
        }
        if spec.num_public_inputs == 0 {
            return invalid_program("public input count must include the claim binding");
        }
        validate_entries(&spec.a, &spec)?;
        validate_entries(&spec.b, &spec)?;
        validate_entries(&spec.c, &spec)?;
        Ok(Self { spec })
    }

    /// Decode and validate a program from bytes.
    pub fn decode(bytes: &Bytes) -> Result<Self, GuestError> {
        let spec = bincode::deserialize::<SpartanR1csProgramSpec>(bytes).map_err(decode_error)?;
        Self::new(spec)
    }

    /// Serialize this program.
    pub fn encode(&self) -> Result<Vec<u8>, GuestError> {
        bincode::serialize(&self.spec).map_err(decode_error)
    }

    fn instance(&self) -> Result<Instance, GuestError> {
        Instance::new(
            usize_from_u32(self.spec.num_constraints, "constraint count")?,
            usize_from_u32(self.spec.num_variables, "variable count")?,
            usize_from_u32(self.spec.num_public_inputs, "public input count")?,
            &matrix_entries(&self.spec.a)?,
            &matrix_entries(&self.spec.b)?,
            &matrix_entries(&self.spec.c)?,
        )
        .map_err(|error| GuestError::ProofProgramInvalid {
            reason: format!("{error:?}"),
        })
    }

    fn gens(&self) -> Result<SNARKGens, GuestError> {
        Ok(SNARKGens::new(
            usize_from_u32(self.spec.num_constraints, "constraint count")?,
            usize_from_u32(self.spec.num_variables, "variable count")?,
            usize_from_u32(self.spec.num_public_inputs, "public input count")?,
            self.max_matrix_entries(),
        ))
    }

    fn public_inputs(
        &self,
        claim: &GuestReceiptClaim,
    ) -> Result<Vec<[u8; SCALAR_BYTES]>, GuestError> {
        let mut outputs = public_outputs_from_bytes(claim.public_output.bytes())?;
        let expected_outputs = self.expected_public_outputs()?;
        if outputs.len() != expected_outputs {
            return Err(GuestError::ProofDataDecode {
                reason: format!(
                    "expected {expected_outputs} public output scalars, got {}",
                    outputs.len()
                ),
            });
        }
        let mut inputs = Vec::with_capacity(outputs.len().saturating_add(1));
        inputs.push(spartan_r1cs_claim_scalar(claim));
        inputs.append(&mut outputs);
        Ok(inputs)
    }

    fn expected_public_outputs(&self) -> Result<usize, GuestError> {
        usize_from_u32(
            self.spec.num_public_inputs.checked_sub(1).ok_or_else(|| {
                GuestError::ProofProgramInvalid {
                    reason: "public input count must include the claim binding".to_string(),
                }
            })?,
            "public output count",
        )
    }

    fn max_matrix_entries(&self) -> usize {
        cmp::max(
            self.spec.a.len(),
            cmp::max(self.spec.b.len(), self.spec.c.len()),
        )
    }

    fn fuel_used(&self) -> u64 {
        u64::from(self.spec.num_constraints)
    }

    fn memory_pages_used(&self) -> u32 {
        let cells = u64::from(self.spec.num_constraints)
            .saturating_add(u64::from(self.spec.num_variables))
            .saturating_add(u64::from(self.spec.num_public_inputs))
            .saturating_add(u64::try_from(self.max_matrix_entries()).unwrap_or(u64::MAX));
        let bytes = cells.saturating_mul(SCALAR_BYTES_U64);
        let pages = bytes.div_ceil(WASM_PAGE_BYTES).max(1);
        u32::try_from(pages).unwrap_or(u32::MAX)
    }
}

impl SpartanR1csVerifier {
    /// Build a verifier for one manifest program hash and program.
    pub fn new(program_hash: super::GuestProgramHash, program: SpartanR1csProgram) -> Self {
        Self {
            program_hash,
            program,
        }
    }

    /// Build a verifier from serialized program bytes.
    pub fn from_binary(
        program_hash: super::GuestProgramHash,
        binary: &GuestBinary,
    ) -> Result<Self, GuestError> {
        let program = SpartanR1csProgram::decode(binary.bytes())?;
        Ok(Self::new(program_hash, program))
    }
}

impl GuestRuntime for SpartanR1csRuntime {
    fn step(&self, input: GuestStepInput) -> Result<GuestStepOutput, GuestError> {
        enforce_in_flight_limits(&self.program, &input)?;
        let witness = decode_witness(input.event.payload.as_ref())?;
        let public_output = GuestPublicOutput::new(flatten_public_outputs(&witness.public_outputs));
        let claim = GuestReceiptClaim::new(
            self.manifest.module_hash(),
            input.public_input.clone(),
            public_output.clone(),
        );
        let proof = prove(&self.program, &claim, &witness)?;
        Ok(GuestStepOutput {
            state: GuestState::new(witness.next_state),
            effects: Vec::new(),
            public_output: public_output.clone(),
            receipt: Some(GuestReceipt {
                program_hash: self.manifest.module_hash(),
                public_input: input.public_input,
                public_output,
                proof: Bytes::from(proof),
            }),
            fuel_used: self.program.fuel_used(),
            memory_pages_used: self.program.memory_pages_used(),
        })
    }
}

impl GuestReceiptVerifier for SpartanR1csVerifier {
    fn verify(&self, claim: &GuestReceiptClaim, receipt: &GuestReceipt) -> Result<(), GuestError> {
        if claim.program_hash != self.program_hash || !claim.matches_receipt(receipt) {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        verify(&self.program, claim, receipt.proof.as_ref())
    }
}

fn prove(
    program: &SpartanR1csProgram,
    claim: &GuestReceiptClaim,
    witness: &SpartanR1csStepWitness,
) -> Result<Vec<u8>, GuestError> {
    let inputs = program.public_inputs(claim)?;
    validate_witness_shape(program, witness, inputs.len())?;
    let instance = program.instance()?;
    let vars = VarsAssignment::new(witness.variables.as_slice()).map_err(spartan_program_error)?;
    let public_inputs = InputsAssignment::new(inputs.as_slice()).map_err(spartan_program_error)?;
    let satisfied = instance
        .is_sat(&vars, &public_inputs)
        .map_err(spartan_program_error)?;
    if !satisfied {
        return Err(GuestError::RuntimeRejected {
            reason: "R1CS witness does not satisfy the guest program".to_string(),
        });
    }
    let gens = program.gens()?;
    let (commitment, decommitment) = SpartanSnark::encode(&instance, &gens);
    let mut transcript = proof_transcript();
    let proof = SpartanSnark::prove(
        &instance,
        &commitment,
        &decommitment,
        vars,
        &public_inputs,
        &gens,
        &mut transcript,
    );
    bincode::serialize(&proof).map_err(|error| GuestError::ProofGenerationFailed {
        reason: error.to_string(),
    })
}

fn verify(
    program: &SpartanR1csProgram,
    claim: &GuestReceiptClaim,
    proof_bytes: &[u8],
) -> Result<(), GuestError> {
    let proof = bincode::deserialize::<SpartanSnark>(proof_bytes).map_err(decode_error)?;
    let inputs = program.public_inputs(claim)?;
    let public_inputs = InputsAssignment::new(inputs.as_slice()).map_err(spartan_program_error)?;
    let instance = program.instance()?;
    let gens = program.gens()?;
    let (commitment, _) = encode_for_verify(&instance, &gens);
    let mut transcript = proof_transcript();
    proof
        .verify(&commitment, &public_inputs, &mut transcript, &gens)
        .map_err(|error| GuestError::ReceiptVerificationFailed {
            reason: format!("{error:?}"),
        })
}

fn encode_for_verify(
    instance: &Instance,
    gens: &SNARKGens,
) -> (ComputationCommitment, ComputationDecommitment) {
    SpartanSnark::encode(instance, gens)
}

fn validate_witness_shape(
    program: &SpartanR1csProgram,
    witness: &SpartanR1csStepWitness,
    public_inputs: usize,
) -> Result<(), GuestError> {
    let expected_vars = usize_from_u32(program.spec.num_variables, "variable count")?;
    if witness.variables.len() != expected_vars {
        return Err(GuestError::ProofDataDecode {
            reason: format!(
                "expected {expected_vars} private variables, got {}",
                witness.variables.len()
            ),
        });
    }
    let expected_inputs = usize_from_u32(program.spec.num_public_inputs, "public input count")?;
    if public_inputs != expected_inputs {
        return Err(GuestError::ProofDataDecode {
            reason: format!("expected {expected_inputs} public inputs, got {public_inputs}"),
        });
    }
    Ok(())
}

fn validate_entries(
    entries: &[SpartanR1csEntry],
    spec: &SpartanR1csProgramSpec,
) -> Result<(), GuestError> {
    let max_col = spec
        .num_variables
        .checked_add(1)
        .and_then(|value| value.checked_add(spec.num_public_inputs))
        .ok_or_else(|| GuestError::ProofProgramInvalid {
            reason: "R1CS dimensions overflow".to_string(),
        })?;
    for entry in entries {
        if entry.row >= spec.num_constraints {
            return invalid_program("matrix entry row is outside the constraint count");
        }
        if entry.col >= max_col {
            return invalid_program("matrix entry column is outside the variable/input count");
        }
        VarsAssignment::new(&[entry.value]).map_err(spartan_program_error)?;
    }
    Ok(())
}

fn matrix_entries(
    entries: &[SpartanR1csEntry],
) -> Result<Vec<(usize, usize, [u8; SCALAR_BYTES])>, GuestError> {
    entries
        .iter()
        .map(|entry| {
            Ok((
                usize_from_u32(entry.row, "matrix row")?,
                usize_from_u32(entry.col, "matrix column")?,
                entry.value,
            ))
        })
        .collect()
}

fn enforce_in_flight_limits(
    program: &SpartanR1csProgram,
    input: &GuestStepInput,
) -> Result<(), GuestError> {
    let fuel_used = program.fuel_used();
    if fuel_used > input.context.fuel_limit {
        return Err(GuestError::FuelLimitExceeded {
            used: fuel_used,
            limit: input.context.fuel_limit,
        });
    }
    let memory_pages_used = program.memory_pages_used();
    if memory_pages_used > input.context.memory_limit_pages {
        return Err(GuestError::MemoryLimitExceeded {
            used: memory_pages_used,
            limit: input.context.memory_limit_pages,
        });
    }
    Ok(())
}

fn require_spartan_profile(manifest: &GuestManifest) -> Result<(), GuestError> {
    if manifest.runtime() == GuestRuntimeKind::R1cs
        && manifest.proof_policy() == ProofPolicy::VerifyReceipt
    {
        return Ok(());
    }
    Err(GuestError::RuntimeRejected {
        reason: "Spartan R1CS adapter requires r1cs + verify_receipt".to_string(),
    })
}

fn decode_witness(bytes: &[u8]) -> Result<SpartanR1csStepWitness, GuestError> {
    bincode::deserialize::<SpartanR1csStepWitness>(bytes).map_err(decode_error)
}

fn public_outputs_from_bytes(bytes: &Bytes) -> Result<Vec<[u8; SCALAR_BYTES]>, GuestError> {
    let chunks = bytes.chunks_exact(SCALAR_BYTES);
    if !chunks.remainder().is_empty() {
        return Err(GuestError::ProofDataDecode {
            reason: "public output length is not a multiple of 32 bytes".to_string(),
        });
    }
    chunks
        .map(|chunk| {
            let mut scalar = [0u8; SCALAR_BYTES];
            scalar.copy_from_slice(chunk);
            Ok(scalar)
        })
        .collect()
}

fn flatten_public_outputs(outputs: &[[u8; SCALAR_BYTES]]) -> Vec<u8> {
    outputs
        .iter()
        .flat_map(|output| output.iter().copied())
        .collect()
}

fn append_len_prefixed(transcript: &mut Vec<u8>, bytes: &Bytes) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    transcript.extend_from_slice(&len.to_le_bytes());
    transcript.extend_from_slice(bytes);
}

fn proof_transcript() -> Transcript {
    Transcript::new(TRANSCRIPT_DOMAIN)
}

fn usize_from_u32(value: u32, field: &'static str) -> Result<usize, GuestError> {
    usize::try_from(value).map_err(|_| GuestError::ProofProgramInvalid {
        reason: format!("{field} does not fit usize"),
    })
}

fn invalid_program<T>(reason: impl Into<String>) -> Result<T, GuestError> {
    Err(GuestError::ProofProgramInvalid {
        reason: reason.into(),
    })
}

fn spartan_program_error(error: impl core::fmt::Debug) -> GuestError {
    GuestError::ProofProgramInvalid {
        reason: format!("{error:?}"),
    }
}

fn decode_error(error: impl ToString) -> GuestError {
    GuestError::ProofDataDecode {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use rings_core::dht::Did;

    use super::*;
    use crate::extension::guest::runtime::accept_step_output;
    use crate::extension::guest::GuestContext;
    use crate::extension::guest::GuestEvent;
    use crate::extension::guest::GuestManifestSpec;
    use crate::extension::guest::GuestProgramHash;
    use crate::extension::guest::GuestPublicInput;
    use crate::extension::guest::GuestRuntimeRegistry;
    use crate::extension::guest::SUPPORTED_GUEST_ABI_VERSION;

    fn scalar(value: u64) -> [u8; SCALAR_BYTES] {
        Scalar::from(value).to_bytes()
    }

    fn entry(row: u32, col: u32, value: [u8; SCALAR_BYTES]) -> SpartanR1csEntry {
        SpartanR1csEntry { row, col, value }
    }

    fn cubic_program() -> SpartanR1csProgram {
        let one = scalar(1);
        let five = scalar(5);
        SpartanR1csProgram::new(SpartanR1csProgramSpec {
            num_constraints: 5,
            num_variables: 5,
            num_public_inputs: 2,
            a: vec![
                entry(0, 0, one),
                entry(1, 1, one),
                entry(2, 2, one),
                entry(2, 0, one),
                entry(3, 3, one),
                entry(3, 5, five),
                entry(4, 4, one),
            ],
            b: vec![
                entry(0, 0, one),
                entry(1, 0, one),
                entry(2, 5, one),
                entry(3, 5, one),
                entry(4, 5, one),
            ],
            c: vec![
                entry(0, 1, one),
                entry(1, 2, one),
                entry(2, 3, one),
                entry(3, 7, one),
                entry(4, 6, one),
            ],
        })
        .expect("valid cubic R1CS")
    }

    fn hash(seed: u8, field: &'static str) -> GuestProgramHash {
        GuestProgramHash::new([seed; SCALAR_BYTES], field).expect("non-zero hash")
    }

    fn manifest_with_limits(fuel_limit: u64, memory_limit: u32) -> GuestManifest {
        GuestManifest::validate(GuestManifestSpec {
            namespace: "guest.r1cs".to_string(),
            runtime: GuestRuntimeKind::R1cs,
            abi_version: SUPPORTED_GUEST_ABI_VERSION,
            module_hash: hash(9, "module_hash"),
            state_schema_hash: hash(2, "state_schema_hash"),
            event_schema_hash: hash(3, "event_schema_hash"),
            effect_schema_hash: hash(4, "effect_schema_hash"),
            capabilities: Vec::new(),
            memory_limit,
            fuel_limit,
            proof_policy: ProofPolicy::VerifyReceipt,
        })
        .expect("valid manifest")
    }

    fn manifest() -> GuestManifest {
        manifest_with_limits(100, 2)
    }

    fn binary(program: &SpartanR1csProgram, manifest: &GuestManifest) -> GuestBinary {
        GuestBinary::new(
            Bytes::from(program.encode().expect("encode program")),
            manifest.module_hash(),
        )
        .expect("valid binary")
    }

    fn public_input() -> GuestPublicInput {
        GuestPublicInput::new(Bytes::from_static(b"host-public-input"))
    }

    fn output_for_x(x: Scalar) -> [u8; SCALAR_BYTES] {
        ((x * x * x) + x + Scalar::from(5u64)).to_bytes()
    }

    fn witness_for(
        manifest: &GuestManifest,
        public_input: GuestPublicInput,
        output: [u8; SCALAR_BYTES],
        x: Scalar,
    ) -> SpartanR1csStepWitness {
        let public_output = GuestPublicOutput::new(flatten_public_outputs(&[output]));
        let claim = GuestReceiptClaim::new(manifest.module_hash(), public_input, public_output);
        let x2 = x * x;
        let x3 = x2 * x;
        let z3 = x3 + x;
        SpartanR1csStepWitness {
            variables: vec![
                x.to_bytes(),
                x2.to_bytes(),
                x3.to_bytes(),
                z3.to_bytes(),
                spartan_r1cs_claim_scalar(&claim),
            ],
            public_outputs: vec![output],
            next_state: Bytes::from_static(b"proved"),
        }
    }

    fn input_for(manifest: &GuestManifest, witness: SpartanR1csStepWitness) -> GuestStepInput {
        GuestStepInput {
            state: GuestState::new(Bytes::from_static(b"state")),
            event: GuestEvent {
                from: Did::from(2u32),
                payload: Bytes::from(bincode::serialize(&witness).expect("encode witness")),
            },
            context: GuestContext::from_manifest(manifest, Did::from(1u32)),
            public_input: public_input(),
        }
    }

    fn proved_output() -> (GuestManifest, SpartanR1csVerifier, GuestStepOutput) {
        let program = cubic_program();
        let manifest = manifest();
        let binary = binary(&program, &manifest);
        let verifier =
            SpartanR1csVerifier::from_binary(manifest.module_hash(), &binary).expect("verifier");
        let runtime = instantiate_spartan_r1cs_runtime(&manifest, binary).expect("runtime");
        let x = Scalar::from(7u64);
        let witness = witness_for(&manifest, public_input(), output_for_x(x), x);
        let output = runtime.step(input_for(&manifest, witness)).expect("prove");
        (manifest, verifier, output)
    }

    #[test]
    fn spartan_r1cs_runtime_produces_receipt_accepted_by_verifier() {
        let (manifest, verifier, output) = proved_output();
        let x = Scalar::from(7u64);
        let witness = witness_for(&manifest, public_input(), output_for_x(x), x);
        let input = input_for(&manifest, witness);

        let accepted =
            accept_step_output(&manifest, &input, output, &verifier).expect("accepted output");
        assert_eq!(
            accepted.state,
            GuestState::new(Bytes::from_static(b"proved"))
        );
        assert!(accepted.effects.is_empty());
    }

    #[test]
    fn spartan_r1cs_verifier_rejects_tampered_proof() {
        let (manifest, verifier, mut output) = proved_output();
        let receipt = output.receipt.as_mut().expect("receipt");
        let mut proof = receipt.proof.to_vec();
        let first = proof.first_mut().expect("proof byte");
        *first ^= 1;
        receipt.proof = Bytes::from(proof);
        let claim = GuestReceiptClaim::new(
            manifest.module_hash(),
            public_input(),
            output.public_output.clone(),
        );

        assert!(matches!(
            verifier.verify(&claim, receipt),
            Err(GuestError::ProofDataDecode { .. })
                | Err(GuestError::ReceiptVerificationFailed { .. })
        ));
    }

    #[test]
    fn spartan_r1cs_verifier_rejects_wrong_public_output() {
        let (manifest, verifier, output) = proved_output();
        let receipt = output.receipt.as_ref().expect("receipt");
        let claim = GuestReceiptClaim::new(
            manifest.module_hash(),
            public_input(),
            GuestPublicOutput::new(flatten_public_outputs(&[scalar(99)])),
        );

        assert_eq!(
            verifier.verify(&claim, receipt),
            Err(GuestError::ReceiptClaimMismatch)
        );
    }

    #[test]
    fn spartan_r1cs_verifier_rejects_mismatched_program_hash() {
        let (_manifest, verifier, output) = proved_output();
        let receipt = output.receipt.as_ref().expect("receipt");
        let claim = GuestReceiptClaim::new(
            hash(8, "module_hash"),
            public_input(),
            output.public_output.clone(),
        );

        assert_eq!(
            verifier.verify(&claim, receipt),
            Err(GuestError::ReceiptClaimMismatch)
        );
    }

    #[test]
    fn spartan_r1cs_runtime_rejects_unsatisfied_witness() {
        let program = cubic_program();
        let manifest = manifest();
        let runtime = instantiate_spartan_r1cs_runtime(&manifest, binary(&program, &manifest))
            .expect("runtime");
        let x = Scalar::from(7u64);
        let bad_output = scalar(42);
        let witness = witness_for(&manifest, public_input(), bad_output, x);

        assert!(matches!(
            runtime.step(input_for(&manifest, witness)),
            Err(GuestError::RuntimeRejected { .. })
        ));
    }

    #[test]
    fn spartan_r1cs_runtime_meters_before_proving() {
        let program = cubic_program();
        let manifest = manifest_with_limits(4, 2);
        let runtime = instantiate_spartan_r1cs_runtime(&manifest, binary(&program, &manifest))
            .expect("runtime");
        let x = Scalar::from(7u64);
        let witness = witness_for(&manifest, public_input(), output_for_x(x), x);

        assert_eq!(
            runtime.step(input_for(&manifest, witness)),
            Err(GuestError::FuelLimitExceeded { used: 5, limit: 4 })
        );
    }

    #[test]
    fn spartan_r1cs_registry_drives_backend_through_generic_profile() {
        let program = cubic_program();
        let manifest = manifest();
        let binary = binary(&program, &manifest);
        let mut registry = GuestRuntimeRegistry::new();
        registry
            .register(spartan_r1cs_runtime_adapter())
            .expect("register adapter");
        let runtime = registry
            .instantiate(&manifest, binary)
            .expect("instantiate through registry");
        let x = Scalar::from(7u64);
        let witness = witness_for(&manifest, public_input(), output_for_x(x), x);

        assert!(runtime.step(input_for(&manifest, witness)).is_ok());
    }
}
