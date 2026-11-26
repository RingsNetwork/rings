//! Connection and transport interfaces with their state and send-admission models.

/// Core transport and connection trait definitions.
mod interface;
/// One-send admission permit and irrevocable send guard types.
mod send;
/// Coherent connection phase and channel readiness state machine.
mod state;

pub use interface::custom_frame_wire;
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc"
))]
pub(crate) use interface::stored_max_message_size;
pub(crate) use interface::BorrowedTransportMessage;
pub use interface::BoxedTransport;
pub use interface::ConnectionInterface;
pub use interface::DynConnection;
pub use interface::DynTransportInterface;
pub use interface::HandshakeStep;
pub use interface::SharedTransport;
pub use interface::TransportInterface;
pub(crate) use interface::TransportMessage;
pub use interface::DEFAULT_MAX_HANDSHAKE_STEP_BYTES;
pub use interface::MAX_CHANNEL_MESSAGE_SIZE;
pub use interface::MAX_HANDSHAKE_STEPS;
pub use interface::MIN_MAX_MESSAGE_SIZE;
pub use send::AcceptedSendPermit;
pub use send::IrrevocableSendGuard;
pub use send::IrrevocableSendPermit;
pub use send::SendAcceptance;
pub use send::SendPermit;
pub use send::SendPermitClaim;
pub use send::CONNECTION_RETIRE_TIMEOUT;
pub use send::IRREVOCABLE_SEND_COMPLETION_TIMEOUT;
pub use state::ConnectionStateCell;
pub use state::ConnectionStateSnapshot;
pub use state::TransportConnectionState;

pub use crate::callback::link_credit::LaneCreditReservation;
pub use crate::callback::link_credit::LaneCreditView;

#[cfg(test)]
mod tests;
