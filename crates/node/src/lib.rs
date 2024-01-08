#![doc = include_str!("../README.md")]
#![cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub mod backend;
pub mod consts;
pub mod error;
pub mod logging;
pub mod measure;
#[cfg(feature = "node")]
pub mod native;
pub mod prelude;
pub mod processor;
pub mod provider;
mod rpc_impl;
pub mod seed;
#[cfg(test)]
mod tests;
pub mod util;
