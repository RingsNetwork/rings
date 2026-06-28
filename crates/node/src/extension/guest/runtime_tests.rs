use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use rings_core::dht::Did;

use super::assert_deterministic_replay;
use super::runtime::accept_step_output;
use super::GuestBinary;
use super::GuestCapability;
use super::GuestContext;
use super::GuestEffect;
use super::GuestError;
use super::GuestEvent;
use super::GuestManifest;
use super::GuestManifestSpec;
use super::GuestProgramHash;
use super::GuestPublicInput;
use super::GuestPublicOutput;
use super::GuestReceipt;
use super::GuestReceiptClaim;
use super::GuestReceiptVerifier;
use super::GuestRuntime;
use super::GuestRuntimeFnAdapter;
use super::GuestRuntimeKind;
use super::GuestRuntimeRegistry;
use super::GuestState;
use super::GuestStepInput;
use super::GuestStepOutput;
use super::ProofPolicy;
use super::SUPPORTED_GUEST_ABI_VERSION;

fn hash(seed: u8, field: &'static str) -> GuestProgramHash {
    GuestProgramHash::new([seed; 32], field).expect("non-zero test hash")
}

fn manifest_with(
    runtime: GuestRuntimeKind,
    capabilities: Vec<GuestCapability>,
    proof_policy: ProofPolicy,
) -> GuestManifest {
    GuestManifest::validate(GuestManifestSpec {
        namespace: "guest.test".to_string(),
        runtime,
        abi_version: SUPPORTED_GUEST_ABI_VERSION,
        module_hash: hash(1, "module_hash"),
        state_schema_hash: hash(2, "state_schema_hash"),
        event_schema_hash: hash(3, "event_schema_hash"),
        effect_schema_hash: hash(4, "effect_schema_hash"),
        capabilities,
        memory_limit: 2,
        fuel_limit: 10,
        proof_policy,
    })
    .expect("valid guest manifest")
}

fn input_for(manifest: &GuestManifest) -> GuestStepInput {
    GuestStepInput {
        state: GuestState::new(Bytes::from_static(b"state")),
        event: GuestEvent {
            from: Did::from(2u32),
            payload: Bytes::from_static(b"event"),
        },
        context: GuestContext::from_manifest(manifest, Did::from(1u32)),
        public_input: GuestPublicInput::new(Bytes::from_static(b"public-input")),
    }
}

fn output_with(effects: Vec<GuestEffect>) -> GuestStepOutput {
    GuestStepOutput {
        state: GuestState::new(Bytes::from_static(b"next")),
        effects,
        public_output: GuestPublicOutput::new(Bytes::from_static(b"public-output")),
        receipt: None,
        fuel_used: 1,
        memory_pages_used: 1,
    }
}

#[test]
fn step_input_abi_round_trip_preserves_value() {
    let manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let input = input_for(&manifest);

    let encoded = input.encode_abi().expect("encode input ABI");
    let decoded = GuestStepInput::decode_abi(encoded.as_slice()).expect("decode input ABI");
    assert_eq!(decoded, input);
}

#[test]
fn step_output_abi_round_trip_preserves_value() {
    let output = output_with(vec![GuestEffect::Inject {
        payload: Bytes::from_static(b"self"),
    }]);

    let encoded = output.encode_abi().expect("encode output ABI");
    let decoded = GuestStepOutput::decode_abi(encoded.as_slice()).expect("decode output ABI");
    assert_eq!(decoded, output);
}

#[test]
fn step_output_abi_rejects_invalid_bytes() {
    assert!(matches!(
        GuestStepOutput::decode_abi(b"not a guest ABI"),
        Err(GuestError::AbiDecode { .. })
    ));
}

struct AllowVerifier;

impl GuestReceiptVerifier for AllowVerifier {
    fn verify(
        &self,
        _claim: &GuestReceiptClaim,
        _receipt: &GuestReceipt,
    ) -> Result<(), GuestError> {
        Ok(())
    }
}

struct DenyVerifier;

impl GuestReceiptVerifier for DenyVerifier {
    fn verify(
        &self,
        _claim: &GuestReceiptClaim,
        _receipt: &GuestReceipt,
    ) -> Result<(), GuestError> {
        Err(GuestError::ReceiptVerificationFailed {
            reason: "test verifier denial".to_string(),
        })
    }
}

#[test]
fn binary_validation_requires_matching_manifest_hash() {
    let manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let binary = GuestBinary::new(Bytes::from_static(b"module"), hash(9, "module_hash")).unwrap();

    assert_eq!(
        binary.validate_manifest(&manifest),
        Err(GuestError::ProgramHashMismatch {
            expected: manifest.module_hash(),
            actual: hash(9, "module_hash"),
        })
    );
}

#[test]
fn output_validation_denies_undeclared_capability() {
    let manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let input = input_for(&manifest);
    let output = output_with(vec![GuestEffect::Send {
        to: Did::from(3u32),
        payload: Bytes::from_static(b"blocked"),
    }]);

    assert_eq!(
        accept_step_output(&manifest, &input, output, &AllowVerifier),
        Err(GuestError::CapabilityDenied {
            capability: GuestCapability::Send
        })
    );
}

#[test]
fn output_validation_accepts_declared_capabilities() {
    let manifest = manifest_with(
        GuestRuntimeKind::Wasm,
        vec![GuestCapability::Send, GuestCapability::Inject],
        ProofPolicy::None,
    );
    let input = input_for(&manifest);
    let effects = vec![
        GuestEffect::Send {
            to: Did::from(3u32),
            payload: Bytes::from_static(b"send"),
        },
        GuestEffect::Inject {
            payload: Bytes::from_static(b"inject"),
        },
    ];
    let output = output_with(effects.clone());

    let accepted =
        accept_step_output(&manifest, &input, output, &AllowVerifier).expect("accepted output");
    assert_eq!(accepted.effects, effects);
    assert_eq!(accepted.state, GuestState::new(Bytes::from_static(b"next")));
}

#[test]
fn output_validation_enforces_fuel_and_memory_limits() {
    let manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let input = input_for(&manifest);

    let mut high_fuel = output_with(Vec::new());
    high_fuel.fuel_used = 11;
    assert_eq!(
        accept_step_output(&manifest, &input, high_fuel, &AllowVerifier),
        Err(GuestError::FuelLimitExceeded {
            used: 11,
            limit: 10
        })
    );

    let mut high_memory = output_with(Vec::new());
    high_memory.memory_pages_used = 3;
    assert_eq!(
        accept_step_output(&manifest, &input, high_memory, &AllowVerifier),
        Err(GuestError::MemoryLimitExceeded { used: 3, limit: 2 })
    );
}

#[test]
fn receipt_policy_requires_matching_verified_receipt() {
    let manifest = manifest_with(
        GuestRuntimeKind::Riscv,
        Vec::new(),
        ProofPolicy::VerifyReceipt,
    );
    let input = input_for(&manifest);

    assert_eq!(
        accept_step_output(&manifest, &input, output_with(Vec::new()), &AllowVerifier),
        Err(GuestError::ReceiptRequired)
    );

    let mut mismatched = output_with(Vec::new());
    mismatched.receipt = Some(GuestReceipt {
        program_hash: hash(9, "module_hash"),
        public_input: input.public_input.clone(),
        public_output: mismatched.public_output.clone(),
        proof: Bytes::from_static(b"proof"),
    });
    assert_eq!(
        accept_step_output(&manifest, &input, mismatched, &AllowVerifier),
        Err(GuestError::ReceiptClaimMismatch)
    );

    let mut denied = output_with(Vec::new());
    denied.receipt = Some(GuestReceipt {
        program_hash: manifest.module_hash(),
        public_input: input.public_input.clone(),
        public_output: denied.public_output.clone(),
        proof: Bytes::from_static(b"proof"),
    });
    assert_eq!(
        accept_step_output(&manifest, &input, denied, &DenyVerifier),
        Err(GuestError::ReceiptVerificationFailed {
            reason: "test verifier denial".to_string()
        })
    );

    let mut verified = output_with(Vec::new());
    verified.receipt = Some(GuestReceipt {
        program_hash: manifest.module_hash(),
        public_input: input.public_input.clone(),
        public_output: verified.public_output.clone(),
        proof: Bytes::from_static(b"proof"),
    });
    assert!(accept_step_output(&manifest, &input, verified, &AllowVerifier).is_ok());
}

struct StaticRuntime {
    output: GuestStepOutput,
}

impl GuestRuntime for StaticRuntime {
    fn step(&self, _input: GuestStepInput) -> Result<GuestStepOutput, GuestError> {
        Ok(self.output.clone())
    }
}

struct CounterRuntime {
    counter: AtomicU8,
}

impl GuestRuntime for CounterRuntime {
    fn step(&self, _input: GuestStepInput) -> Result<GuestStepOutput, GuestError> {
        let value = self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(GuestStepOutput {
            state: GuestState::new(Bytes::from(vec![value])),
            effects: Vec::new(),
            public_output: GuestPublicOutput::new(Bytes::from(vec![value])),
            receipt: None,
            fuel_used: 1,
            memory_pages_used: 1,
        })
    }
}

fn wasm_test_factory(
    _manifest: &GuestManifest,
    _binary: GuestBinary,
) -> Result<Box<dyn GuestRuntime>, GuestError> {
    Ok(Box::new(StaticRuntime {
        output: GuestStepOutput {
            state: GuestState::new(Bytes::from_static(b"wasm")),
            effects: Vec::new(),
            public_output: GuestPublicOutput::new(Bytes::from_static(b"wasm")),
            receipt: None,
            fuel_used: 1,
            memory_pages_used: 1,
        },
    }))
}

fn riscv_test_factory(
    _manifest: &GuestManifest,
    _binary: GuestBinary,
) -> Result<Box<dyn GuestRuntime>, GuestError> {
    Ok(Box::new(StaticRuntime {
        output: GuestStepOutput {
            state: GuestState::new(Bytes::from_static(b"riscv")),
            effects: Vec::new(),
            public_output: GuestPublicOutput::new(Bytes::from_static(b"riscv")),
            receipt: None,
            fuel_used: 1,
            memory_pages_used: 1,
        },
    }))
}

#[test]
fn deterministic_replay_returns_the_replayed_output() {
    let manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let runtime = StaticRuntime {
        output: output_with(Vec::new()),
    };

    let output =
        assert_deterministic_replay(&runtime, input_for(&manifest)).expect("deterministic runtime");
    assert_eq!(output.state, GuestState::new(Bytes::from_static(b"next")));
}

#[test]
fn deterministic_replay_rejects_divergent_output() {
    let manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let runtime = CounterRuntime {
        counter: AtomicU8::new(0),
    };

    assert_eq!(
        assert_deterministic_replay(&runtime, input_for(&manifest)),
        Err(GuestError::NonDeterministicOutput)
    );
}

#[test]
fn runtime_registry_selects_wasm_and_riscv_adapters_from_manifest() {
    let wasm_manifest = manifest_with(GuestRuntimeKind::Wasm, Vec::new(), ProofPolicy::None);
    let riscv_manifest = manifest_with(
        GuestRuntimeKind::Riscv,
        Vec::new(),
        ProofPolicy::VerifyReceipt,
    );
    let binary = GuestBinary::new(Bytes::from_static(b"module"), wasm_manifest.module_hash())
        .expect("guest binary");
    let mut registry = GuestRuntimeRegistry::new();
    registry
        .register(GuestRuntimeFnAdapter::new(
            GuestRuntimeKind::Wasm,
            wasm_test_factory
                as fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
        ))
        .expect("register wasm adapter");
    registry
        .register(GuestRuntimeFnAdapter::new(
            GuestRuntimeKind::Riscv,
            riscv_test_factory
                as fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>,
        ))
        .expect("register riscv adapter");

    let wasm = registry
        .instantiate(&wasm_manifest, binary.clone())
        .expect("instantiate wasm");
    assert_eq!(
        wasm.step(input_for(&wasm_manifest))
            .expect("wasm step")
            .state,
        GuestState::new(Bytes::from_static(b"wasm"))
    );

    let riscv = registry
        .instantiate(&riscv_manifest, binary)
        .expect("instantiate riscv");
    assert_eq!(
        riscv
            .step(input_for(&riscv_manifest))
            .expect("riscv step")
            .state,
        GuestState::new(Bytes::from_static(b"riscv"))
    );
}

#[test]
fn runtime_registry_rejects_unregistered_runtime() {
    let manifest = manifest_with(
        GuestRuntimeKind::Riscv,
        Vec::new(),
        ProofPolicy::VerifyReceipt,
    );
    let binary =
        GuestBinary::new(Bytes::from_static(b"module"), manifest.module_hash()).expect("binary");
    let registry = GuestRuntimeRegistry::new();

    assert!(matches!(
        registry.instantiate(&manifest, binary),
        Err(GuestError::RuntimeUnavailable {
            runtime: GuestRuntimeKind::Riscv
        })
    ));
}

#[test]
fn runtime_registry_rejects_duplicate_adapter_kind() {
    let mut registry = GuestRuntimeRegistry::new();
    let factory = wasm_test_factory
        as fn(&GuestManifest, GuestBinary) -> Result<Box<dyn GuestRuntime>, GuestError>;

    registry
        .register(GuestRuntimeFnAdapter::new(GuestRuntimeKind::Wasm, factory))
        .expect("first wasm adapter registration succeeds");

    assert_eq!(
        registry.register(GuestRuntimeFnAdapter::new(GuestRuntimeKind::Wasm, factory)),
        Err(GuestError::DuplicateRuntimeAdapter {
            runtime: GuestRuntimeKind::Wasm
        })
    );
}
