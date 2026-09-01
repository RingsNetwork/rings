//! Internal events routed through a dummy connection.

use super::controlled;
use crate::callback::AdmittedInboundFrame;
use crate::core::transport::WebrtcConnectionState;

pub(super) enum Event {
    PeerConnectionStateChange(WebrtcConnectionState, Option<String>),
    DataChannelOpen(Option<String>),
    DataChannelClose(Option<String>),
    Message(AdmittedInboundFrame),
}

impl Event {
    pub(super) fn inspect(&self) -> controlled::QueuedDeliveryKind {
        match self {
            Self::PeerConnectionStateChange(state, _) => {
                controlled::QueuedDeliveryKind::PeerConnectionStateChange(*state)
            }
            Self::DataChannelOpen(_) => controlled::QueuedDeliveryKind::DataChannelOpen,
            Self::DataChannelClose(_) => controlled::QueuedDeliveryKind::DataChannelClose,
            Self::Message(frame) => {
                controlled::QueuedDeliveryKind::Message(frame.payload().clone())
            }
        }
    }

    pub(super) fn is_lifecycle_event(&self) -> bool {
        !matches!(self, Self::Message(_))
    }

    pub(super) fn set_callback_cid(&mut self, cid: String) {
        match self {
            Self::PeerConnectionStateChange(_, callback_cid)
            | Self::DataChannelOpen(callback_cid)
            | Self::DataChannelClose(callback_cid) => {
                *callback_cid = Some(cid);
            }
            Self::Message(_) => {}
        }
    }
}
