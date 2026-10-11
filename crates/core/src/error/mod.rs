//! Error of rings_core

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Error of a transport callback (see [`crate::swarm::callback::InnerSwarmCallback`]).
///
/// Native callbacks require `Send + Sync` because callback work is driven by Tokio tasks;
/// browser callbacks may retain thread-local error values.
pub type CallbackError = Box<rings_runtime::maybe_send_sync!(dyn std::error::Error)>;

mod kind;
mod policy;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
mod wasm;

pub use kind::Error;
