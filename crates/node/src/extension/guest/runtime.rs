#![warn(missing_docs)]
//! Runtime-neutral guest execution model.

use std::collections::BTreeMap;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use super::GuestCapability;
use super::GuestManifest;
use super::GuestProgramHash;
use super::GuestRuntimeKind;
use super::ProofPolicy;
use crate::extension::ext::MaybeSend;

/// Guest extension error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuestError {
    /// Manifest namespace is empty.
    #[error("guest namespace is empty")]
    EmptyNamespace,
    /// Manifest namespace contains unsupported characters.
    #[error("guest namespace {namespace:?} contains unsupported characters")]
    InvalidNamespace {
        /// Invalid namespace.
        namespace: String,
    },
    /// Manifest ABI version is not supported.
    #[error("unsupported guest ABI version {actual}; supported version is {supported}")]
    UnsupportedAbiVersion {
        /// Supported ABI version.
        supported: u16,
        /// Actual ABI version.
        actual: u16,
    },
    /// A required hash is all zero bytes.
    #[error("guest manifest hash {field} is empty")]
    EmptyHash {
        /// Manifest field.
        field: &'static str,
    },
    /// Memory limit is zero.
    #[error("guest memory limit must be non-zero")]
    ZeroMemoryLimit,
    /// Fuel limit is zero.
    #[error("guest fuel limit must be non-zero")]
    ZeroFuelLimit,
    /// A manifest declared the same capability twice.
    #[error("guest capability {capability:?} is declared more than once")]
    DuplicateCapability {
        /// Duplicated capability.
        capability: GuestCapability,
    },
    /// A guest binary hash did not match the manifest module hash.
    #[error("guest binary hash does not match manifest module hash")]
    ProgramHashMismatch {
        /// Manifest module hash.
        expected: GuestProgramHash,
        /// Binary hash.
        actual: GuestProgramHash,
    },
    /// A guest binary has no bytes.
    #[error("guest binary is empty")]
    EmptyBinary,
    /// No runtime adapter is registered for the requested profile.
    #[error("no guest runtime registered for {profile:?}")]
    RuntimeUnavailable {
        /// Requested runtime profile.
        profile: GuestRuntimeProfile,
    },
    /// Runtime adapter registration conflicts with an existing profile.
    #[error("guest runtime adapter for {profile:?} is already registered")]
    DuplicateRuntimeAdapter {
        /// Duplicated runtime profile.
        profile: GuestRuntimeProfile,
    },
    /// The runtime rejected the transition.
    #[error("guest runtime rejected transition: {reason}")]
    RuntimeRejected {
        /// Adapter-specific reason.
        reason: String,
    },
    /// A proving runtime rejected its encoded program.
    #[error("guest proof program is invalid: {reason}")]
    ProofProgramInvalid {
        /// Validation reason.
        reason: String,
    },
    /// A proving runtime failed to decode a program, witness, or receipt.
    #[error("guest proof data decode failed: {reason}")]
    ProofDataDecode {
        /// Decoding reason.
        reason: String,
    },
    /// A proving runtime failed while producing a proof.
    #[error("guest proof generation failed: {reason}")]
    ProofGenerationFailed {
        /// Prover reason.
        reason: String,
    },
    /// Guest output used more fuel than its manifest allows.
    #[error("guest used {used} fuel but manifest allows {limit}")]
    FuelLimitExceeded {
        /// Used fuel.
        used: u64,
        /// Limit.
        limit: u64,
    },
    /// Guest output used more memory than its manifest allows.
    #[error("guest used {used} memory pages but manifest allows {limit}")]
    MemoryLimitExceeded {
        /// Used memory pages.
        used: u32,
        /// Limit.
        limit: u32,
    },
    /// Guest emitted an effect not declared by its manifest.
    #[error("guest emitted undeclared capability {capability:?}")]
    CapabilityDenied {
        /// Denied capability.
        capability: GuestCapability,
    },
    /// Proof policy requires a receipt but the output did not include one.
    #[error("guest receipt is required by manifest proof policy")]
    ReceiptRequired,
    /// Receipt claim does not match the accepted transition.
    #[error("guest receipt claim does not match transition output")]
    ReceiptClaimMismatch,
    /// Receipt verifier rejected the proof.
    #[error("guest receipt verifier rejected proof: {reason}")]
    ReceiptVerificationFailed {
        /// Verifier reason.
        reason: String,
    },
    /// The same runtime and input produced different outputs.
    #[error("guest runtime produced non-deterministic output")]
    NonDeterministicOutput,
    /// Guest ABI encoding failed.
    #[error("guest ABI encode failed: {reason}")]
    AbiEncode {
        /// Encoding error.
        reason: String,
    },
    /// Guest ABI decoding failed.
    #[error("guest ABI decode failed: {reason}")]
    AbiDecode {
        /// Decoding error.
        reason: String,
    },
}

/// Raw guest binary plus the loader's program hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestBinary {
    bytes: Bytes,
    hash: GuestProgramHash,
}

impl GuestBinary {
    /// Build a guest binary.
    pub fn new(bytes: Bytes, hash: GuestProgramHash) -> Result<Self, GuestError> {
        if bytes.is_empty() {
            return Err(GuestError::EmptyBinary);
        }
        Ok(Self { bytes, hash })
    }

    /// Raw binary bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Program hash supplied by the loader.
    pub fn hash(&self) -> GuestProgramHash {
        self.hash
    }

    /// Validate this binary against a manifest's module hash.
    pub fn validate_manifest(&self, manifest: &GuestManifest) -> Result<(), GuestError> {
        if self.hash != manifest.module_hash() {
            return Err(GuestError::ProgramHashMismatch {
                expected: manifest.module_hash(),
                actual: self.hash,
            });
        }
        Ok(())
    }
}

/// Opaque guest state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestState(Bytes);

impl GuestState {
    /// Build a guest state from bytes.
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self(bytes.into())
    }

    /// Return state bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.0
    }
}

/// Opaque public input committed into a proof receipt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestPublicInput(Bytes);

impl GuestPublicInput {
    /// Build public input bytes.
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self(bytes.into())
    }

    /// Borrow public input bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.0
    }
}

/// Opaque public output committed into a proof receipt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestPublicOutput(Bytes);

impl GuestPublicOutput {
    /// Build public output bytes.
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self(bytes.into())
    }

    /// Borrow public output bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.0
    }
}

/// Decoded event given to a guest runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestEvent {
    /// Authenticated sender.
    pub from: Did,
    /// Opaque guest event payload.
    pub payload: Bytes,
}

/// Host context visible to a guest transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestContext {
    /// Local DID.
    pub did: Did,
    /// Guest module hash.
    pub program_hash: GuestProgramHash,
    /// Runtime family selected by the manifest.
    pub runtime: GuestRuntimeKind,
    /// Memory limit in 64KiB pages.
    pub memory_limit_pages: u32,
    /// Fuel limit for one transition.
    pub fuel_limit: u64,
}

impl GuestContext {
    /// Build context from a manifest and local DID.
    pub fn from_manifest(manifest: &GuestManifest, did: Did) -> Self {
        Self {
            did,
            program_hash: manifest.module_hash(),
            runtime: manifest.runtime(),
            memory_limit_pages: manifest.limits().memory_pages().get(),
            fuel_limit: manifest.limits().fuel().get(),
        }
    }
}

/// Guest transition input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestStepInput {
    /// Current guest state.
    pub state: GuestState,
    /// Guest event.
    pub event: GuestEvent,
    /// Host context.
    pub context: GuestContext,
    /// Public input committed into a receipt.
    pub public_input: GuestPublicInput,
}

impl GuestStepInput {
    /// Encode this input with the v1 guest ABI.
    pub fn encode_abi(&self) -> Result<Vec<u8>, GuestError> {
        encode_abi(self)
    }

    /// Decode an input encoded with the v1 guest ABI.
    pub fn decode_abi(bytes: &[u8]) -> Result<Self, GuestError> {
        decode_abi(bytes)
    }
}

/// Host effects a v1 guest may request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestEffect {
    /// Send payload to `to` under the manifest namespace.
    Send {
        /// Destination peer.
        to: Did,
        /// Payload to send.
        payload: Bytes,
    },
    /// Re-inject payload into the manifest namespace with local provenance.
    Inject {
        /// Payload to re-enter through this guest namespace.
        payload: Bytes,
    },
}

impl GuestEffect {
    /// Capability required to execute this effect.
    pub fn capability(&self) -> GuestCapability {
        match self {
            Self::Send { .. } => GuestCapability::Send,
            Self::Inject { .. } => GuestCapability::Inject,
        }
    }
}

/// Proof receipt returned by a proving guest runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestReceipt {
    /// Program hash this proof is bound to.
    pub program_hash: GuestProgramHash,
    /// Public input committed by the proof.
    pub public_input: GuestPublicInput,
    /// Public output committed by the proof.
    pub public_output: GuestPublicOutput,
    /// Backend-specific proof bytes.
    pub proof: Bytes,
}

/// Receipt claim the host expects for an accepted transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestReceiptClaim {
    /// Program hash expected by the manifest.
    pub program_hash: GuestProgramHash,
    /// Public input for this transition.
    pub public_input: GuestPublicInput,
    /// Public output for this transition.
    pub public_output: GuestPublicOutput,
}

impl GuestReceiptClaim {
    /// Build the expected receipt claim.
    pub fn new(
        program_hash: GuestProgramHash,
        public_input: GuestPublicInput,
        public_output: GuestPublicOutput,
    ) -> Self {
        Self {
            program_hash,
            public_input,
            public_output,
        }
    }

    /// Whether a receipt carries exactly this public claim.
    pub fn matches_receipt(&self, receipt: &GuestReceipt) -> bool {
        self.program_hash == receipt.program_hash
            && self.public_input == receipt.public_input
            && self.public_output == receipt.public_output
    }
}

/// Verifier for backend-specific guest receipts.
pub trait GuestReceiptVerifier: MaybeSend {
    /// Verify `receipt` against the expected public claim.
    fn verify(&self, claim: &GuestReceiptClaim, receipt: &GuestReceipt) -> Result<(), GuestError>;
}

/// Guest runtime transition output before host validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestStepOutput {
    /// Next guest state.
    pub state: GuestState,
    /// Effects requested by the guest.
    pub effects: Vec<GuestEffect>,
    /// Public output committed into a receipt.
    pub public_output: GuestPublicOutput,
    /// Optional proof receipt.
    pub receipt: Option<GuestReceipt>,
    /// Fuel consumed by this transition.
    pub fuel_used: u64,
    /// Memory pages used by this transition.
    pub memory_pages_used: u32,
}

impl GuestStepOutput {
    /// Encode this output with the v1 guest ABI.
    pub fn encode_abi(&self) -> Result<Vec<u8>, GuestError> {
        encode_abi(self)
    }

    /// Decode an output encoded with the v1 guest ABI.
    pub fn decode_abi(bytes: &[u8]) -> Result<Self, GuestError> {
        decode_abi(bytes)
    }
}

/// Host-accepted transition output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestAcceptedOutput {
    /// Next guest state.
    pub state: GuestState,
    /// Effects permitted by the manifest.
    pub effects: Vec<GuestEffect>,
}

/// Runtime-neutral guest execution interface.
///
/// Manifest resource limits are authoritative. Concrete adapters must meter and
/// trap in flight using the limits supplied in [`GuestContext`]; host-side output
/// validation is only a backstop for reported usage.
pub trait GuestRuntime: MaybeSend {
    /// Execute one deterministic guest transition.
    fn step(&self, input: GuestStepInput) -> Result<GuestStepOutput, GuestError>;
}

/// Runtime adapter capability selected by a manifest.
///
/// The runtime family and proof policy are independent dimensions. For example,
/// a WebAssembly adapter may be deterministic-replay only or may produce a
/// receipt through a proving backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GuestRuntimeProfile {
    runtime: GuestRuntimeKind,
    proof_policy: ProofPolicy,
}

impl GuestRuntimeProfile {
    /// Build a runtime profile.
    pub fn new(runtime: GuestRuntimeKind, proof_policy: ProofPolicy) -> Self {
        Self {
            runtime,
            proof_policy,
        }
    }

    /// Build the profile requested by a validated manifest.
    pub fn from_manifest(manifest: &GuestManifest) -> Self {
        Self::new(manifest.runtime(), manifest.proof_policy())
    }

    /// Runtime family served by this profile.
    pub fn runtime(&self) -> GuestRuntimeKind {
        self.runtime
    }

    /// Receipt policy served by this profile.
    pub fn proof_policy(&self) -> ProofPolicy {
        self.proof_policy
    }
}

/// Runtime adapter able to instantiate a concrete guest runtime.
pub trait GuestRuntimeAdapter: MaybeSend {
    /// Runtime and proof capability this adapter serves.
    fn profile(&self) -> GuestRuntimeProfile;

    /// Instantiate a guest runtime from a validated manifest and matching binary.
    fn instantiate(
        &self,
        manifest: &GuestManifest,
        binary: GuestBinary,
    ) -> Result<Box<dyn GuestRuntime>, GuestError>;
}

/// Function-backed runtime adapter tagged by [`GuestRuntimeProfile`].
pub struct GuestRuntimeFnAdapter<F> {
    profile: GuestRuntimeProfile,
    factory: F,
}

impl<F> GuestRuntimeFnAdapter<F> {
    /// Build a runtime adapter from its profile and instantiation function.
    pub fn new(profile: GuestRuntimeProfile, factory: F) -> Self {
        Self { profile, factory }
    }
}

impl<F> GuestRuntimeAdapter for GuestRuntimeFnAdapter<F>
where F: for<'a> Fn(&'a GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>
        + MaybeSend
{
    fn profile(&self) -> GuestRuntimeProfile {
        self.profile
    }

    fn instantiate(
        &self,
        manifest: &GuestManifest,
        binary: GuestBinary,
    ) -> Result<Box<dyn GuestRuntime>, GuestError> {
        (self.factory)(manifest, binary)
    }
}

/// Runtime adapter registry keyed by manifest runtime and proof capability.
#[derive(Default)]
pub struct GuestRuntimeRegistry {
    adapters: BTreeMap<GuestRuntimeProfile, Box<dyn GuestRuntimeAdapter>>,
}

impl GuestRuntimeRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one runtime/profile adapter.
    pub fn register<A>(&mut self, adapter: A) -> Result<(), GuestError>
    where A: GuestRuntimeAdapter + 'static {
        let profile = adapter.profile();
        if self.adapters.contains_key(&profile) {
            return Err(GuestError::DuplicateRuntimeAdapter { profile });
        }
        self.adapters.insert(profile, Box::new(adapter));
        Ok(())
    }

    /// Instantiate the runtime selected by `manifest`.
    pub fn instantiate(
        &self,
        manifest: &GuestManifest,
        binary: GuestBinary,
    ) -> Result<Box<dyn GuestRuntime>, GuestError> {
        binary.validate_manifest(manifest)?;
        let profile = GuestRuntimeProfile::from_manifest(manifest);
        let Some(adapter) = self.adapters.get(&profile) else {
            return Err(GuestError::RuntimeUnavailable { profile });
        };
        adapter.instantiate(manifest, binary)
    }
}

/// Validate a runtime output and return host-accepted state plus effects.
pub fn accept_step_output<V>(
    manifest: &GuestManifest,
    input: &GuestStepInput,
    output: GuestStepOutput,
    verifier: &V,
) -> Result<GuestAcceptedOutput, GuestError>
where
    V: GuestReceiptVerifier,
{
    validate_resource_limits(manifest, &output)?;
    validate_capabilities(manifest, output.effects.as_slice())?;
    validate_receipt_policy(manifest, input, &output, verifier)?;
    Ok(GuestAcceptedOutput {
        state: output.state,
        effects: output.effects,
    })
}

/// Run a runtime twice with the same input and require identical outputs.
pub fn assert_deterministic_replay<R>(
    runtime: &R,
    input: GuestStepInput,
) -> Result<GuestStepOutput, GuestError>
where
    R: GuestRuntime,
{
    let first = runtime.step(input.clone())?;
    let second = runtime.step(input)?;
    if first != second {
        return Err(GuestError::NonDeterministicOutput);
    }
    Ok(first)
}

fn validate_resource_limits(
    manifest: &GuestManifest,
    output: &GuestStepOutput,
) -> Result<(), GuestError> {
    let limits = manifest.limits();
    let fuel_limit = limits.fuel().get();
    if output.fuel_used > fuel_limit {
        return Err(GuestError::FuelLimitExceeded {
            used: output.fuel_used,
            limit: fuel_limit,
        });
    }
    let memory_limit = limits.memory_pages().get();
    if output.memory_pages_used > memory_limit {
        return Err(GuestError::MemoryLimitExceeded {
            used: output.memory_pages_used,
            limit: memory_limit,
        });
    }
    Ok(())
}

fn validate_capabilities(
    manifest: &GuestManifest,
    effects: &[GuestEffect],
) -> Result<(), GuestError> {
    for effect in effects {
        let capability = effect.capability();
        if !manifest.permits(capability) {
            return Err(GuestError::CapabilityDenied { capability });
        }
    }
    Ok(())
}

fn validate_receipt_policy<V>(
    manifest: &GuestManifest,
    input: &GuestStepInput,
    output: &GuestStepOutput,
    verifier: &V,
) -> Result<(), GuestError>
where
    V: GuestReceiptVerifier,
{
    if manifest.proof_policy() != ProofPolicy::VerifyReceipt {
        return Ok(());
    }
    let Some(receipt) = output.receipt.as_ref() else {
        return Err(GuestError::ReceiptRequired);
    };
    let claim = GuestReceiptClaim::new(
        manifest.module_hash(),
        input.public_input.clone(),
        output.public_output.clone(),
    );
    if !claim.matches_receipt(receipt) {
        return Err(GuestError::ReceiptClaimMismatch);
    }
    verifier.verify(&claim, receipt)
}

fn encode_abi<T>(value: &T) -> Result<Vec<u8>, GuestError>
where T: Serialize {
    bincode::serialize(value).map_err(|error| GuestError::AbiEncode {
        reason: error.to_string(),
    })
}

fn decode_abi<T>(bytes: &[u8]) -> Result<T, GuestError>
where T: for<'de> Deserialize<'de> {
    bincode::deserialize(bytes).map_err(|error| GuestError::AbiDecode {
        reason: error.to_string(),
    })
}
