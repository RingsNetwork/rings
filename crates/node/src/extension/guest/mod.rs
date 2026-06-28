#![warn(missing_docs)]
//! Dynamically loaded guest extensions.
//!
//! A guest extension is a deterministic state transition loaded behind the existing
//! extension boundary:
//!
//! ```text
//! step : (State, Event, Context) -> Result<(State, Effects), GuestError>
//! ```
//!
//! The guest program returns effects as data. [`GuestShell`] is the only host boundary that
//! interprets those effects, and it receives the normal namespace-scoped
//! [`Scope`](crate::extension::ext::Scope), so a guest can only send or inject under the
//! namespace fixed by its [`GuestManifest`].
//!
//! This module is not the existing SNARK witness-WASM path. The SNARK crate runs Circom
//! witness calculators as part of proof generation. Guest extensions are a general dynamic
//! extension layer: the manifest validates a loadable program, runtime adapters execute the
//! deterministic transition, and the host shell interprets only declared capabilities.

mod protocol;

pub use protocol::register_guest_extension;
pub use protocol::GuestProtocol;
pub use protocol::GuestProtocolState;
pub use protocol::GuestShell;
pub use rings_runtime::assert_deterministic_replay;
#[cfg(all(feature = "guest-riscv-prove", not(target_arch = "wasm32")))]
pub use rings_runtime::instantiate_risc0_zkvm_runtime;
#[cfg(feature = "guest-riscv-proof")]
pub use rings_runtime::risc0_image_id_hash;
#[cfg(all(feature = "guest-riscv-prove", not(target_arch = "wasm32")))]
pub use rings_runtime::risc0_zkvm_runtime_adapter;
#[cfg(feature = "guest-wasm-proof")]
pub use rings_runtime::zkwasm_aggregator_claim_scalar;
pub use rings_runtime::GuestAcceptedOutput;
pub use rings_runtime::GuestBinary;
pub use rings_runtime::GuestCapabilities;
pub use rings_runtime::GuestCapability;
pub use rings_runtime::GuestContext;
pub use rings_runtime::GuestEffect;
pub use rings_runtime::GuestError;
pub use rings_runtime::GuestEvent;
pub use rings_runtime::GuestManifest;
pub use rings_runtime::GuestManifestSpec;
pub use rings_runtime::GuestProgramHash;
pub use rings_runtime::GuestPublicInput;
pub use rings_runtime::GuestPublicOutput;
pub use rings_runtime::GuestReceipt;
pub use rings_runtime::GuestReceiptClaim;
pub use rings_runtime::GuestReceiptVerifier;
pub use rings_runtime::GuestResourceLimits;
pub use rings_runtime::GuestRuntime;
pub use rings_runtime::GuestRuntimeAdapter;
pub use rings_runtime::GuestRuntimeFnAdapter;
pub use rings_runtime::GuestRuntimeKind;
pub use rings_runtime::GuestRuntimeProfile;
pub use rings_runtime::GuestRuntimeRegistry;
pub use rings_runtime::GuestState;
pub use rings_runtime::GuestStepInput;
pub use rings_runtime::GuestStepOutput;
pub use rings_runtime::ProofPolicy;
#[cfg(feature = "guest-riscv-proof")]
pub use rings_runtime::Risc0ReceiptVerifier;
#[cfg(all(feature = "guest-riscv-prove", not(target_arch = "wasm32")))]
pub use rings_runtime::Risc0ZkvmRuntime;
#[cfg(all(feature = "guest-riscv-prove", not(target_arch = "wasm32")))]
pub use rings_runtime::Risc0ZkvmRuntimeAdapter;
#[cfg(feature = "guest-wasm-proof")]
pub use rings_runtime::ZkWasmAggregatorReceipt;
#[cfg(feature = "guest-wasm-proof")]
pub use rings_runtime::ZkWasmAggregatorVerifier;
pub use rings_runtime::SUPPORTED_GUEST_ABI_VERSION;
