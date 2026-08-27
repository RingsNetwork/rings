use super::effective_max_message_size;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use super::ConnectionStateCell;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use super::ConnectionStateSnapshot;
use super::IrrevocableSendGuard;
use super::SendPermit;
use super::TransportMessage;
use super::WebrtcConnectionState;
use super::MAX_DATA_CHANNEL_MESSAGE_SIZE;

mod send_permit;
mod state;
