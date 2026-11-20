//! Internal events routed through a dummy connection.

use super::controlled;
use crate::callback::AdmittedInboundFrame;
use crate::core::pool::ChannelLane;
use crate::core::transport::TransportConnectionState;

pub(super) enum Event {
    ConnectionStateChange(TransportConnectionState, Option<String>),
    ChannelOpen(Option<String>),
    ChannelClose(Option<String>),
    /// A frame admitted against the receiver's credit at send, and the lane it travels on.
    Message(AdmittedInboundFrame, ChannelLane),
}

impl Event {
    pub(super) fn inspect(&self) -> controlled::QueuedDeliveryKind {
        match self {
            Self::ConnectionStateChange(state, _) => {
                controlled::QueuedDeliveryKind::ConnectionStateChange(*state)
            }
            Self::ChannelOpen(_) => controlled::QueuedDeliveryKind::ChannelOpen,
            Self::ChannelClose(_) => controlled::QueuedDeliveryKind::ChannelClose,
            Self::Message(frame, lane) => {
                controlled::QueuedDeliveryKind::Message(frame.payload().clone(), *lane)
            }
        }
    }

    pub(super) fn is_lifecycle_event(&self) -> bool {
        !matches!(self, Self::Message(..))
    }

    pub(super) fn set_callback_cid(&mut self, cid: String) {
        match self {
            Self::ConnectionStateChange(_, callback_cid)
            | Self::ChannelOpen(callback_cid)
            | Self::ChannelClose(callback_cid) => {
                *callback_cid = Some(cid);
            }
            Self::Message(..) => {}
        }
    }
}
