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

/// Platform-dependent thread-safety bound used by transport adapters.
///
/// Native transports require `Send + Sync`; single-threaded browser transports
/// do not. This trait keeps that target distinction out of duplicated impls.
#[doc(hidden)]
#[cfg(not(all(feature = "web-sys-webrtc", target_family = "wasm")))]
pub trait PlatformSendSync: Send + Sync {}

#[doc(hidden)]
#[cfg(not(all(feature = "web-sys-webrtc", target_family = "wasm")))]
impl<T: Send + Sync + ?Sized> PlatformSendSync for T {}

/// Platform-dependent thread-safety bound used by transport adapters.
#[doc(hidden)]
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub trait PlatformSendSync {}

#[doc(hidden)]
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
impl<T: ?Sized> PlatformSendSync for T {}
