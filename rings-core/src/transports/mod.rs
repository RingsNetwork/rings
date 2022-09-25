//! Transport about `wasm` and `node`, use `dummy` for testing
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub mod default;
#[cfg(all(not(feature = "wasm"), feature = "dummy"))]
pub mod dummy;
#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub use default::DefaultTransport as Transport;
#[cfg(all(not(feature = "wasm"), feature = "dummy"))]
pub use dummy::DummyTransport as Transport;
#[cfg(feature = "wasm")]
pub use wasm::WasmTransport as Transport;

pub mod helper;
pub mod manager;
