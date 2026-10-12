//! Error of rings_core

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

mod kind;
mod policy;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
mod wasm;

pub use kind::Error;
