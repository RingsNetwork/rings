//! Abstract transport interfaces and credit-bounded channel algebra:
//!
//! - [`ConnectionInterface`](transport::ConnectionInterface) defines the two-party offer/answer
//!   handshake and framed message transport over ordered lanes.
//! - [`TransportInterface`](transport::TransportInterface) and
//!   [`DynTransportInterface`](transport::DynTransportInterface) manage the lifecycle of peer
//!   connections.
//! - [`TransportCallback`](callback::TransportCallback) receives connection lifecycle and
//!   credit-admitted inbound message events.

pub mod admission;
pub mod callback;
pub mod credit;
pub mod drop_guard;
pub mod pool;
pub mod transport;

#[cfg(any(
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
pub(crate) mod send;
