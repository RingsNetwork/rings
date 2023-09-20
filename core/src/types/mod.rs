//! Core traits, including Channel, IceTransport

/// Channel trait and message.
pub mod channel;

use rings_transport::connection_ref::ConnectionRef;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcConnection as ConnectionOwner;
#[cfg(not(feature = "wasm"))]
pub use rings_transport::connections::WebrtcConnection as ConnectionOwner;

pub type Connection = ConnectionRef<ConnectionOwner>;
