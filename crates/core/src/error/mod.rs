//! Error of rings_core

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Application callback error retained as the source of a core error.
///
/// Wasm callbacks may retain thread-local error values.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type CallbackError = Box<dyn std::error::Error>;

/// Application callback error retained as the source of a core error.
///
/// Since 0.18 native callbacks require `Send + Sync` because callback work is
/// driven by Tokio tasks. Prefer this alias in [`crate::swarm::callback::SwarmCallback`]
/// implementations instead of spelling the trait object directly.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type CallbackError = Box<dyn std::error::Error + Send + Sync>;

mod kind;
mod policy;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
mod wasm;

pub use kind::Error;
