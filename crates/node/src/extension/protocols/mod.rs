#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in is a `(Protocol, Interpret)` pair registered under its namespace. The
//! relay's pure model is one generic [`relay::Relay`]; only its interpreter differs by
//! platform (`NativeRelay` / `WtRelay`).

pub mod echo;
#[cfg(feature = "browser")]
pub mod js;
pub mod relay;
