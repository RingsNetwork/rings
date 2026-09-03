//! Utils for ring-core.

mod id;
mod time;

pub(crate) use id::new_uuid;
pub use time::get_epoch_ms;
pub(crate) use time::get_epoch_ms_i64;
pub(crate) use time::sleep;
pub(crate) use time::try_sleep;
pub(crate) use time::Instant;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript function wrappers used by the WASM bindings.
pub mod js_func;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript interop helpers used by the WASM bindings.
pub mod js_utils;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript value conversion helpers used by the WASM bindings.
pub mod js_value;

#[cfg(all(test, not(target_family = "wasm")))]
mod generation_witness;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use generation_witness::GenerationWitness;
