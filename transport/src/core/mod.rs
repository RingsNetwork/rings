//! The main concepts of this mod are:
//!
//! The [ConnectionInterface](transport::ConnectionInterface) trait defines how to
//! make webrtc ice handshake with a remote peer and then send data channel message to it.
//! See the [transport] module.
//!
//! The [ConnectionCreation](transport::ConnectionCreation) trait should be
//! implemented for each Transport<Connection>. See the [transport] module.
//!
//! The [Callback](callback::Callback) trait is used to let user handle
//! the events of a connection, including connection state change,
//! coming data channel message and etc. See the [callback] module.

pub mod callback;
pub mod transport;
