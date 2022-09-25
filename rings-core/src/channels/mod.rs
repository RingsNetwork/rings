//! Async channel for both browser(wasm) and native

#[cfg(not(feature = "wasm"))]
mod default;
#[cfg(feature = "wasm")]
mod wasm;

#[cfg(not(feature = "wasm"))]
pub use default::AcChannel as Channel;
#[cfg(feature = "wasm")]
pub use wasm::CbChannel as Channel;
