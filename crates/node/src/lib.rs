#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]
/// Shared protocol constants used by the node runtime.
pub mod consts;
mod descriptor;
pub mod error;
pub mod extension;
pub mod logging;
pub mod measure;
#[cfg(feature = "node")]
/// Native-node configuration, CLI, and runtime adapters.
pub mod native;
pub mod onion;
pub mod online;
mod peer_quota;
pub mod prelude;
pub mod processor;
pub mod provider;
pub mod registration;
mod rpc_dto;
mod rpc_impl;
pub mod seed;
mod sync_lock;
#[cfg(all(test, rings_native))]
mod test_support;
#[cfg(test)]
mod tests;
pub mod util;
