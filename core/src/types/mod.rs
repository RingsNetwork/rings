//! Core traits, including Channel, IceTransport

use crate::err::Result;

/// Channel trait and message.
pub mod channel;
/// Custom webrtc ice_server and traits.
pub mod ice_transport;
/// MessageListener trait use for MessageHandler.
pub mod message;

/// A trait that defines methods for verifying and fixing data.
pub trait Fixable {
    /// Type of data needs for fix
    type Data;
    /// Performs a "dry run" verification of the data represented by the implementing type.
    fn verify(&self) -> Result<Self::Data>;

    /// Performs a full verification of the data represented by the implementing type,
    /// potentially modifying it if necessary to correct any issues that are found.
    fn fix(&mut self);
}
