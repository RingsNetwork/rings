//! Core traits, including Channel, IceTransport

/// Channel trait and message.
pub mod channel;

use rings_transport::connection_ref::ConnectionRef;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyConnection as ConnectionOwner;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyTransport as Transport;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcConnection as ConnectionOwner;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcTransport as Transport;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub use rings_transport::connections::WebrtcConnection as ConnectionOwner;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub use rings_transport::connections::WebrtcTransport as Transport;

pub type Connection = ConnectionRef<ConnectionOwner>;
