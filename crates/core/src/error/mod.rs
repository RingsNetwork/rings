//! Error of rings_core

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

mod kind;
mod policy;
mod send_class;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
mod wasm;

pub use kind::DetachedWork;
pub use kind::Error;
pub(crate) use send_class::DeferralTrigger;
pub(crate) use send_class::SendClass;
pub use send_class::SendDeferral;
