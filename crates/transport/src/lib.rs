#![warn(missing_docs)]
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

#[cfg(all(
    feature = "web-sys-webrtc",
    any(feature = "dummy", feature = "native-webrtc")
))]
compile_error!(
    "rings-transport feature `web-sys-webrtc` cannot be combined with native transport features"
);

pub mod callback;
pub mod connection_ref;
pub mod connections;
pub mod core;
pub mod delivery;
pub mod error;
pub mod ice_server;
pub mod notifier;
pub mod pool;
pub mod webrtc_config;

mod sync_utils;
