use rings_transport::core::callback::InboundFrameClass;

use crate::message::MessageClass;
use crate::message::MessageMeta;

pub(super) const INBOUND_LANE_COUNT: usize = MessageClass::COUNT + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InboundLane(usize);

impl InboundLane {
    pub(super) const REASSEMBLY: Self = Self(MessageClass::COUNT);

    pub(super) const fn from_class(class: MessageClass) -> Self {
        Self(class.index())
    }

    pub(super) const fn from_meta(meta: MessageMeta) -> Self {
        if meta.kind().is_chunk() {
            return Self::REASSEMBLY;
        }
        Self::from_class(meta.class())
    }

    pub(super) const fn from_frame_class(class: InboundFrameClass) -> Self {
        match class {
            InboundFrameClass::Control => Self::from_class(MessageClass::DhtControl),
            InboundFrameClass::Storage => Self::from_class(MessageClass::Storage),
            InboundFrameClass::EndToEnd => Self::from_class(MessageClass::E2e),
            InboundFrameClass::Application | InboundFrameClass::Data => {
                Self::from_class(MessageClass::Application)
            }
            InboundFrameClass::Reassembly => Self::REASSEMBLY,
        }
    }

    pub(super) const fn index(self) -> usize {
        self.0
    }

    pub(super) const fn from_index(index: usize) -> Option<Self> {
        if index < INBOUND_LANE_COUNT {
            Some(Self(index))
        } else {
            None
        }
    }

    pub(super) fn is_logical_data(self) -> bool {
        self != Self::from_class(MessageClass::DhtControl) && self != Self::REASSEMBLY
    }
}
