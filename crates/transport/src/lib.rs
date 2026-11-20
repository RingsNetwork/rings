#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]
#![doc = include_str!("../README.md")]

pub mod callback;
pub mod connection_pool;
pub mod connection_ref;
pub mod connections;
pub mod core;
pub mod delivery;
pub mod error;
#[cfg(feature = "dummy")]
#[doc(hidden)]
pub mod hot_path;
pub mod ice_server;
pub mod network_config;
pub mod notifier;
pub mod sdp;

mod sync_utils;
