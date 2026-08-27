//! Capacity-bounded dispatch from transport backends to protocol callbacks.

mod capacity;
mod inner;
mod invalid_report;

#[cfg(any(test, feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(crate) use capacity::admit_inbound_data_channel;
pub(crate) use capacity::inbound_frame_exceeds_protocol_ceiling;
#[cfg(all(test, feature = "dummy"))]
pub(crate) use capacity::inbound_peer_frame_capacity_for_test;
pub use capacity::AdmittedInboundFrame;
pub use capacity::InboundFrameAdmission;
pub use capacity::InboundFrameCapacity;
pub(crate) use capacity::InboundFramePermit;
#[cfg(test)]
use capacity::INBOUND_DATA_CHANNEL_CAPACITY;
#[cfg(test)]
use capacity::INBOUND_FRAME_CAPACITY;
#[cfg(test)]
use capacity::INBOUND_PEER_BYTE_CAPACITY;
#[cfg(test)]
use capacity::INBOUND_PEER_FRAME_CAPACITY;
pub use inner::InnerTransportCallback;
#[cfg(all(test, not(target_family = "wasm")))]
use invalid_report::INVALID_FRAME_REPORT_BACKLOG_CAPACITY;
#[cfg(all(test, not(target_family = "wasm")))]
use invalid_report::INVALID_FRAME_REPORT_QUANTUM;

#[cfg(test)]
mod tests;
