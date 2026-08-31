//! Utils for ring-core.

mod time;

pub use time::get_epoch_ms;
pub(crate) use time::get_epoch_ms_i64;
pub(crate) use time::sleep;
pub(crate) use time::try_sleep;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript function wrappers used by the WASM bindings.
pub mod js_func;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript interop helpers used by the WASM bindings.
pub mod js_utils;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript value conversion helpers used by the WASM bindings.
pub mod js_value;
