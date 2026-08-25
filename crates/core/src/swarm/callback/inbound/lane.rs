use rings_transport::core::callback::InboundFrameClass;

use crate::message::MessageClass;
use crate::message::MessageMeta;

pub(super) const INBOUND_LANE_COUNT: usize = MessageClass::COUNT + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InboundLane {
    DhtControl,
    Storage,
    E2e,
    Application,
    Reassembly,
}

impl InboundLane {
    pub(super) const DHT_CONTROL: Self = Self::DhtControl;
    pub(super) const STORAGE: Self = Self::Storage;
    pub(super) const E2E: Self = Self::E2e;
    pub(super) const APPLICATION: Self = Self::Application;
    pub(super) const REASSEMBLY: Self = Self::Reassembly;
    pub(super) const ALL: [Self; INBOUND_LANE_COUNT] = [
        Self::DHT_CONTROL,
        Self::STORAGE,
        Self::E2E,
        Self::APPLICATION,
        Self::REASSEMBLY,
    ];

    pub(super) const fn from_class(class: MessageClass) -> Self {
        match class {
            MessageClass::DhtControl => Self::DHT_CONTROL,
            MessageClass::Storage => Self::STORAGE,
            MessageClass::E2e => Self::E2E,
            MessageClass::Application => Self::APPLICATION,
        }
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
        match self {
            Self::DhtControl => 0,
            Self::Storage => 1,
            Self::E2e => 2,
            Self::Application => 3,
            Self::Reassembly => MessageClass::COUNT,
        }
    }

    pub(super) const fn is_logical_data(self) -> bool {
        matches!(self, Self::Storage | Self::E2e | Self::Application)
    }
}
