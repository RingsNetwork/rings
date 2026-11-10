//! Built-in protocol extensions.
//!
//! Each built-in is a `(Protocol, Interpret)` pair registered under its namespace. The
//! relay's pure model is one generic [`relay::Relay`], interpreted natively by `NativeRelay`.

pub mod echo;
#[cfg(rings_browser)]
pub mod js;
#[cfg(any(rings_browser, test))]
pub(crate) mod outbox;
#[cfg(rings_native)]
pub mod relay;
