//! Unix packet-device configuration and privilege-helper protocol.

mod client;
pub mod config;
#[doc(hidden)]
pub mod helper;
pub mod lease;
mod transport;

pub use client::UnixTunnelControl;
pub use client::UnixTunnelLease;
pub use client::UnixTunnelOptions;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
