//! Error of rings_core

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

mod detached_work;
/// Core error enum definition.
mod kind;
/// Error classification and retry policy helpers.
mod policy;
mod send_class;
/// WebAssembly `JsValue` conversions for [`Error`].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
mod wasm;

pub use detached_work::DetachedWork;
pub use kind::Error;
pub(crate) use send_class::DeferralTrigger;
pub(crate) use send_class::SendClass;
pub use send_class::SendDeferral;
