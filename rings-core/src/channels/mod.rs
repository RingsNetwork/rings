//! Async channel for both browser(wasm) and native(node).

#[cfg(not(feature = "wasm"))]
mod default;
#[cfg(feature = "wasm")]
mod wasm;

/// A Channel build with wasm feature or not.
/// - wasm features: using mpsc::UnboundedSender and mpsc::UnboundedReceiver.
/// - default fatures: using async_channel::Sender and async_channel::Receiver.
#[cfg(not(feature = "wasm"))]
pub use default::AcChannel as Channel;
#[cfg(feature = "wasm")]
pub use wasm::CbChannel as Channel;
