#![warn(missing_docs)]
//! Guest extension runtime model and proof adapters.
//!
//! This crate owns the runtime-neutral guest state-transition model, manifest
//! validation, receipt contract, and thin proof adapters. `rings-node` should
//! only bind this model to the node extension registry and host `Scope`.

mod manifest;
#[cfg(any(feature = "risc0-proof", feature = "wasm-proof"))]
mod proofs;
#[cfg(feature = "risc0-proof")]
mod risc0;
mod runtime;
#[cfg(test)]
mod runtime_tests;
#[cfg(feature = "wasm-proof")]
mod zkwasm;

pub use manifest::GuestCapabilities;
pub use manifest::GuestCapability;
pub use manifest::GuestManifest;
pub use manifest::GuestManifestSpec;
pub use manifest::GuestProgramHash;
pub use manifest::GuestResourceLimits;
pub use manifest::GuestRuntimeKind;
pub use manifest::ProofPolicy;
pub use manifest::SUPPORTED_GUEST_ABI_VERSION;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use risc0::instantiate_risc0_zkvm_runtime;
#[cfg(feature = "risc0-proof")]
pub use risc0::risc0_image_id_hash;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use risc0::risc0_zkvm_runtime_adapter;
#[cfg(feature = "risc0-proof")]
pub use risc0::Risc0ReceiptVerifier;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use risc0::Risc0ZkvmRuntime;
#[cfg(all(feature = "risc0-prove", not(target_arch = "wasm32")))]
pub use risc0::Risc0ZkvmRuntimeAdapter;
pub use runtime::accept_step_output;
pub use runtime::assert_deterministic_replay;
pub use runtime::GuestAcceptedOutput;
pub use runtime::GuestBinary;
pub use runtime::GuestContext;
pub use runtime::GuestEffect;
pub use runtime::GuestError;
pub use runtime::GuestEvent;
pub use runtime::GuestPublicInput;
pub use runtime::GuestPublicOutput;
pub use runtime::GuestReceipt;
pub use runtime::GuestReceiptClaim;
pub use runtime::GuestReceiptVerifier;
pub use runtime::GuestRuntime;
pub use runtime::GuestRuntimeAdapter;
pub use runtime::GuestRuntimeFnAdapter;
pub use runtime::GuestRuntimeProfile;
pub use runtime::GuestRuntimeRegistry;
pub use runtime::GuestState;
pub use runtime::GuestStepInput;
pub use runtime::GuestStepOutput;
#[cfg(feature = "wasm-proof")]
pub use zkwasm::zkwasm_aggregator_claim_scalar;
#[cfg(feature = "wasm-proof")]
pub use zkwasm::ZkWasmAggregatorReceipt;
#[cfg(feature = "wasm-proof")]
pub use zkwasm::ZkWasmAggregatorVerifier;

/// Auto-trait bound that is `Send + Sync` on native and empty on browser.
///
/// Runtime traits use this bound so pure guest model code can compile for both
/// native and browser targets without leaking node extension registry details.
#[cfg(not(feature = "browser"))]
pub trait MaybeSend: Send + Sync {}
#[cfg(not(feature = "browser"))]
impl<T: Send + Sync> MaybeSend for T {}

/// Auto-trait bound that is `Send + Sync` on native and empty on browser.
#[cfg(feature = "browser")]
pub trait MaybeSend {}
#[cfg(feature = "browser")]
impl<T> MaybeSend for T {}
