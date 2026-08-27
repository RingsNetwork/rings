//! Connection and transport interfaces with their state and send-admission models.

mod interface;
mod send;
mod state;

pub use interface::effective_max_message_size;
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc"
))]
pub(crate) use interface::stored_max_message_size;
pub(crate) use interface::BorrowedTransportMessage;
pub use interface::BoxedTransport;
pub use interface::ConnectionInterface;
pub use interface::TransportInterface;
pub use interface::TransportMessage;
pub use interface::MAX_DATA_CHANNEL_MESSAGE_SIZE;
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
pub(crate) use send::IrrevocableSendGuard;
pub use send::IrrevocableSendPermit;
pub use send::SendAcceptance;
pub use send::SendPermit;
pub use send::SendPermitClaim;
pub use send::CONNECTION_RETIRE_TIMEOUT;
pub use send::IRREVOCABLE_SEND_COMPLETION_TIMEOUT;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(crate) use state::ConnectionStateCell;
pub use state::ConnectionStateSnapshot;
pub use state::WebrtcConnectionState;

#[cfg(test)]
mod tests;
