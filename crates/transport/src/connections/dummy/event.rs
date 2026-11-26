//! Internal events routed through a dummy connection.

use super::controlled;
use crate::callback::AdmittedInboundFrame;
use crate::core::pool::ChannelLane;
use crate::core::transport::TransportConnectionState;

/// Internal event delivered to a dummy connection's event handler.
pub(super) enum Event {
    /// Connection state transition with an optional overridden callback connection ID.
    ConnectionStateChange(TransportConnectionState, Option<String>),
    /// Data-channel-open notification with an optional overridden callback connection ID.
    ChannelOpen(Option<String>),
    /// Data-channel-close notification with an optional overridden callback connection ID.
    ChannelClose(Option<String>),
    /// A frame admitted against the receiver's credit at send, and the lane it travels on.
    Message(AdmittedInboundFrame, ChannelLane),
}

impl Event {
    /// Produce a read-only controlled-delivery inspection snapshot of this event.
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

    /// Return `true` if this event is a connection or channel lifecycle event rather than a message.
    pub(super) fn is_lifecycle_event(&self) -> bool {
        !matches!(self, Self::Message(..))
    }

    /// Override the connection ID passed to lifecycle callbacks when this event is delivered.
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
