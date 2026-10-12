//! Credit-bounded dispatch from transport backends to protocol callbacks.

mod capacity;
#[cfg(rings_transport_webrtc)]
pub(crate) mod credit_pump;
mod inner;
mod invalid_report;
pub(crate) mod link_credit;
#[cfg(rings_transport_backend)]
pub(crate) mod link_send;

#[cfg(any(test, rings_transport_webrtc))]
pub(crate) use capacity::admit_inbound_data_channel;
#[cfg(any(test, rings_transport_webrtc))]
pub(crate) use capacity::data_channel_label;
pub(crate) use capacity::inbound_frame_exceeds_protocol_ceiling;
pub use capacity::AdmittedInboundFrame;
pub use capacity::InboundFrameAdmission;
pub use capacity::INBOUND_PEER_FRAME_CAPACITY;
pub use inner::InnerTransportCallback;
#[cfg(all(test, not(target_family = "wasm")))]
use invalid_report::INVALID_FRAME_REPORT_BACKLOG_CAPACITY;
#[cfg(all(test, not(target_family = "wasm")))]
use invalid_report::INVALID_FRAME_REPORT_QUANTUM;
pub use link_credit::NodeReceiveLoad;

#[cfg(test)]
mod tests;
