//! Transport about `wasm` and `node`, use `dummy` for testing
/// Default transport use for node.
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub mod default;
/// Dummy transport use for test.
#[cfg(all(not(feature = "wasm"), feature = "dummy"))]
pub mod dummy;
/// Wasm transport use for browser.
#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub use default::DefaultTransport as Transport;
#[cfg(all(not(feature = "wasm"), feature = "dummy"))]
pub use dummy::DummyTransport as Transport;
#[cfg(feature = "wasm")]
pub use wasm::WasmTransport as Transport;

/// Custom Promise act like Js Promise.
pub mod helper;
/// TransportManager trait and implement.
pub mod manager;
