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

#[cfg(all(
    feature = "native-webrtc",
    target_family = "wasm",
    not(feature = "web-sys-webrtc")
))]
compile_error!(
    "`rings-transport/native-webrtc` is not supported on wasm targets without `web-sys-webrtc`."
);

#[cfg(all(
    feature = "dummy",
    target_family = "wasm",
    not(feature = "web-sys-webrtc")
))]
compile_error!(
    "`rings-transport/dummy` requires the native Tokio runtime and is not supported on wasm targets without `web-sys-webrtc`."
);

#[cfg(all(
    feature = "web-sys-webrtc",
    not(target_family = "wasm"),
    not(any(feature = "native-webrtc", feature = "dummy"))
))]
compile_error!(
    "`rings-transport/web-sys-webrtc` is only supported on wasm targets; enable `native-webrtc` or `dummy` on native targets."
);

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
