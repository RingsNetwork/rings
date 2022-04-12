#[cfg(not(feature = "wasm"))]
mod default;
#[cfg(feature = "wasm")]
mod wasm;

#[cfg(feature = "wasm")]
pub use wasm::CbChannel as Channel;

#[cfg(not(feature = "wasm"))]
pub use default::AcChannel as Channel;
