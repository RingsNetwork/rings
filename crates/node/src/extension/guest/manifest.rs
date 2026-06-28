#![warn(missing_docs)]
//! Guest manifest validation.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;

use super::GuestError;

/// Guest ABI version understood by this crate.
pub const SUPPORTED_GUEST_ABI_VERSION: u16 = 1;

/// Runtime family requested by a guest manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestRuntimeKind {
    /// WebAssembly guest runtime.
    Wasm,
    /// RISC-V guest runtime, intended for zkVM-friendly adapters.
    Riscv,
}

/// Host capabilities a v1 guest may request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestCapability {
    /// Send a payload to a peer under the manifest namespace.
    Send,
    /// Re-inject a payload into the manifest namespace with local provenance.
    Inject,
}

/// Receipt policy for a guest transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofPolicy {
    /// No proof receipt is required for accepting a transition.
    None,
    /// A valid receipt must be verified before accepting a transition.
    VerifyReceipt,
}

/// A 32-byte commitment to guest code or schemas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GuestProgramHash([u8; 32]);

impl GuestProgramHash {
    /// Build a non-zero program or schema hash.
    pub fn new(bytes: [u8; 32], field: &'static str) -> Result<Self, GuestError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(GuestError::EmptyHash { field });
        }
        Ok(Self(bytes))
    }

    /// Return the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Validated v1 capability set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestCapabilities {
    declared: BTreeSet<GuestCapability>,
}

impl GuestCapabilities {
    /// Build a capability set, rejecting duplicate declarations.
    pub fn new(
        capabilities: impl IntoIterator<Item = GuestCapability>,
    ) -> Result<Self, GuestError> {
        let mut declared = BTreeSet::new();
        for capability in capabilities {
            if !declared.insert(capability) {
                return Err(GuestError::DuplicateCapability { capability });
            }
        }
        Ok(Self { declared })
    }

    /// Whether this manifest declares `capability`.
    pub fn permits(&self, capability: GuestCapability) -> bool {
        self.declared.contains(&capability)
    }

    /// Iterate declared capabilities in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = GuestCapability> + '_ {
        self.declared.iter().copied()
    }
}

/// Non-zero resource limits enforced after every guest transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestResourceLimits {
    memory_pages: NonZeroU32,
    fuel: NonZeroU64,
}

impl GuestResourceLimits {
    /// Build resource limits from raw manifest values.
    pub fn new(memory_pages: u32, fuel: u64) -> Result<Self, GuestError> {
        let Some(memory_pages) = NonZeroU32::new(memory_pages) else {
            return Err(GuestError::ZeroMemoryLimit);
        };
        let Some(fuel) = NonZeroU64::new(fuel) else {
            return Err(GuestError::ZeroFuelLimit);
        };
        Ok(Self { memory_pages, fuel })
    }

    /// Maximum guest memory, in WebAssembly-style 64KiB pages.
    pub fn memory_pages(&self) -> NonZeroU32 {
        self.memory_pages
    }

    /// Maximum fuel units accepted for one transition.
    pub fn fuel(&self) -> NonZeroU64 {
        self.fuel
    }
}

/// Raw manifest data supplied by a loader before validation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuestManifestSpec {
    /// Protocol namespace owned by this guest extension.
    pub namespace: String,
    /// Runtime family used to execute the guest binary.
    pub runtime: GuestRuntimeKind,
    /// Guest ABI version.
    pub abi_version: u16,
    /// Hash of the guest module.
    pub module_hash: GuestProgramHash,
    /// Hash of the serialized state schema.
    pub state_schema_hash: GuestProgramHash,
    /// Hash of the serialized event schema.
    pub event_schema_hash: GuestProgramHash,
    /// Hash of the serialized effect schema.
    pub effect_schema_hash: GuestProgramHash,
    /// Capabilities requested by the guest.
    pub capabilities: Vec<GuestCapability>,
    /// Memory limit in 64KiB pages.
    pub memory_limit: u32,
    /// Fuel limit for one transition.
    pub fuel_limit: u64,
    /// Proof policy for transition acceptance.
    pub proof_policy: ProofPolicy,
}

/// Validated guest manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestManifest {
    namespace: String,
    runtime: GuestRuntimeKind,
    abi_version: u16,
    module_hash: GuestProgramHash,
    state_schema_hash: GuestProgramHash,
    event_schema_hash: GuestProgramHash,
    effect_schema_hash: GuestProgramHash,
    capabilities: GuestCapabilities,
    limits: GuestResourceLimits,
    proof_policy: ProofPolicy,
}

impl GuestManifest {
    /// Validate a raw manifest spec.
    pub fn validate(spec: GuestManifestSpec) -> Result<Self, GuestError> {
        validate_namespace(spec.namespace.as_str())?;
        if spec.abi_version != SUPPORTED_GUEST_ABI_VERSION {
            return Err(GuestError::UnsupportedAbiVersion {
                supported: SUPPORTED_GUEST_ABI_VERSION,
                actual: spec.abi_version,
            });
        }
        let capabilities = GuestCapabilities::new(spec.capabilities)?;
        let limits = GuestResourceLimits::new(spec.memory_limit, spec.fuel_limit)?;
        Ok(Self {
            namespace: spec.namespace,
            runtime: spec.runtime,
            abi_version: spec.abi_version,
            module_hash: spec.module_hash,
            state_schema_hash: spec.state_schema_hash,
            event_schema_hash: spec.event_schema_hash,
            effect_schema_hash: spec.effect_schema_hash,
            capabilities,
            limits,
            proof_policy: spec.proof_policy,
        })
    }

    /// Namespace this guest is registered under.
    pub fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    /// Runtime family selected by the manifest.
    pub fn runtime(&self) -> GuestRuntimeKind {
        self.runtime
    }

    /// Guest ABI version.
    pub fn abi_version(&self) -> u16 {
        self.abi_version
    }

    /// Guest module hash.
    pub fn module_hash(&self) -> GuestProgramHash {
        self.module_hash
    }

    /// State schema hash.
    pub fn state_schema_hash(&self) -> GuestProgramHash {
        self.state_schema_hash
    }

    /// Event schema hash.
    pub fn event_schema_hash(&self) -> GuestProgramHash {
        self.event_schema_hash
    }

    /// Effect schema hash.
    pub fn effect_schema_hash(&self) -> GuestProgramHash {
        self.effect_schema_hash
    }

    /// Declared host capabilities.
    pub fn capabilities(&self) -> &GuestCapabilities {
        &self.capabilities
    }

    /// Resource limits.
    pub fn limits(&self) -> GuestResourceLimits {
        self.limits
    }

    /// Proof policy.
    pub fn proof_policy(&self) -> ProofPolicy {
        self.proof_policy
    }

    /// Whether the manifest permits `capability`.
    pub fn permits(&self, capability: GuestCapability) -> bool {
        self.capabilities.permits(capability)
    }
}

fn validate_namespace(namespace: &str) -> Result<(), GuestError> {
    if namespace.is_empty() {
        return Err(GuestError::EmptyNamespace);
    }
    if !namespace.chars().all(is_namespace_char) {
        return Err(GuestError::InvalidNamespace {
            namespace: namespace.to_string(),
        });
    }
    Ok(())
}

fn is_namespace_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8, field: &'static str) -> GuestProgramHash {
        GuestProgramHash::new([seed; 32], field).expect("non-zero test hash")
    }

    fn valid_spec() -> GuestManifestSpec {
        GuestManifestSpec {
            namespace: "guest.example".to_string(),
            runtime: GuestRuntimeKind::Wasm,
            abi_version: SUPPORTED_GUEST_ABI_VERSION,
            module_hash: hash(1, "module_hash"),
            state_schema_hash: hash(2, "state_schema_hash"),
            event_schema_hash: hash(3, "event_schema_hash"),
            effect_schema_hash: hash(4, "effect_schema_hash"),
            capabilities: vec![GuestCapability::Send, GuestCapability::Inject],
            memory_limit: 16,
            fuel_limit: 1_000,
            proof_policy: ProofPolicy::None,
        }
    }

    #[test]
    fn manifest_validation_rejects_empty_namespace() {
        let mut spec = valid_spec();
        spec.namespace.clear();

        assert_eq!(
            GuestManifest::validate(spec),
            Err(GuestError::EmptyNamespace)
        );
    }

    #[test]
    fn manifest_validation_rejects_invalid_namespace_characters() {
        let mut spec = valid_spec();
        spec.namespace = "guest/example".to_string();

        assert_eq!(
            GuestManifest::validate(spec),
            Err(GuestError::InvalidNamespace {
                namespace: "guest/example".to_string()
            })
        );
    }

    #[test]
    fn manifest_validation_rejects_unsupported_abi_version() {
        let mut spec = valid_spec();
        spec.abi_version = SUPPORTED_GUEST_ABI_VERSION + 1;

        assert_eq!(
            GuestManifest::validate(spec),
            Err(GuestError::UnsupportedAbiVersion {
                supported: SUPPORTED_GUEST_ABI_VERSION,
                actual: SUPPORTED_GUEST_ABI_VERSION + 1,
            })
        );
    }

    #[test]
    fn manifest_validation_rejects_duplicate_capabilities() {
        let mut spec = valid_spec();
        spec.capabilities = vec![GuestCapability::Send, GuestCapability::Send];

        assert_eq!(
            GuestManifest::validate(spec),
            Err(GuestError::DuplicateCapability {
                capability: GuestCapability::Send
            })
        );
    }

    #[test]
    fn manifest_validation_rejects_zero_limits() {
        let mut zero_memory = valid_spec();
        zero_memory.memory_limit = 0;
        assert_eq!(
            GuestManifest::validate(zero_memory),
            Err(GuestError::ZeroMemoryLimit)
        );

        let mut zero_fuel = valid_spec();
        zero_fuel.fuel_limit = 0;
        assert_eq!(
            GuestManifest::validate(zero_fuel),
            Err(GuestError::ZeroFuelLimit)
        );
    }

    #[test]
    fn manifest_validation_preserves_declared_capability_set() {
        let manifest = GuestManifest::validate(valid_spec()).expect("valid guest manifest");

        assert!(manifest.permits(GuestCapability::Send));
        assert!(manifest.permits(GuestCapability::Inject));
        assert_eq!(manifest.capabilities().iter().collect::<Vec<_>>(), vec![
            GuestCapability::Send,
            GuestCapability::Inject
        ]);
    }
}
