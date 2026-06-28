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

mod manifest;
mod protocol;
mod runtime;
#[cfg(test)]
mod runtime_tests;

pub use manifest::GuestCapabilities;
pub use manifest::GuestCapability;
pub use manifest::GuestManifest;
pub use manifest::GuestManifestSpec;
pub use manifest::GuestProgramHash;
pub use manifest::GuestResourceLimits;
pub use manifest::GuestRuntimeKind;
pub use manifest::ProofPolicy;
pub use manifest::SUPPORTED_GUEST_ABI_VERSION;
pub use protocol::register_guest_extension;
pub use protocol::GuestProtocol;
pub use protocol::GuestProtocolState;
pub use protocol::GuestShell;
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
