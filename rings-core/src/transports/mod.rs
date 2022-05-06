#[cfg(not(feature = "wasm"))]
pub mod default;
#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(not(feature = "wasm"))]
pub use default::DefaultTransport as Transport;
#[cfg(feature = "wasm")]
pub use wasm::WasmTransport as Transport;

pub mod helper;
