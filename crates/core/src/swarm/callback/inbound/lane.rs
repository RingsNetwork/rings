use crate::message::MessageCategory;
use crate::message::MessageKind;

pub(super) const INBOUND_LANE_COUNT: usize = MessageCategory::COUNT + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InboundLane {
    DhtControl,
    Storage,
    E2e,
    Application,
    Reassembly,
}

impl InboundLane {
    pub(super) const ALL: [Self; INBOUND_LANE_COUNT] = [
        Self::DhtControl,
        Self::Storage,
        Self::E2e,
        Self::Application,
        Self::Reassembly,
    ];

    pub(super) const fn from_class(class: MessageCategory) -> Self {
        match class {
            MessageCategory::DhtControl => Self::DhtControl,
            MessageCategory::Storage => Self::Storage,
            MessageCategory::E2e => Self::E2e,
            MessageCategory::Application => Self::Application,
        }
    }

    pub(crate) const fn from_kind(kind: MessageKind) -> Self {
        if kind.is_chunk() {
            return Self::Reassembly;
        }
        Self::from_class(kind.class())
    }

    pub(super) const fn index(self) -> usize {
        match self {
            Self::DhtControl => 0,
            Self::Storage => 1,
            Self::E2e => 2,
            Self::Application => 3,
            Self::Reassembly => MessageCategory::COUNT,
        }
    }

    pub(super) const fn is_logical_data(self) -> bool {
        matches!(self, Self::Storage | Self::E2e | Self::Application)
    }

    /// The message class this lane carries; the reassembly lane's class is
    /// unknown until the reassembled message is decoded.
    pub(in crate::swarm::callback) const fn class(self) -> Option<MessageCategory> {
        match self {
            Self::DhtControl => Some(MessageCategory::DhtControl),
            Self::Storage => Some(MessageCategory::Storage),
            Self::E2e => Some(MessageCategory::E2e),
            Self::Application => Some(MessageCategory::Application),
            Self::Reassembly => None,
        }
    }
}

const fn lanes_follow_indices(lanes: &[InboundLane], index: usize) -> bool {
    match lanes.split_first() {
        None => true,
        Some((lane, remaining)) => {
            lane.index() == index && lanes_follow_indices(remaining, index + 1)
        }
    }
}

const _: () = assert!(lanes_follow_indices(&InboundLane::ALL, 0));
