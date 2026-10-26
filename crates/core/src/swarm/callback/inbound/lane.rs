use rings_transport::core::pool::ChannelLane;

use crate::message::MessageCategory;
use crate::message::MessageKind;

pub(super) const INBOUND_LANE_COUNT: usize = MessageCategory::COUNT + 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

    /// The lane of the class whose channel lane is `carrier` (one lane per class), or the
    /// reassembly lane for no carrier: the lane a chunked message's first chunk is charged on,
    /// so the completed message is dispatched on the lane its reservation sits in (#932).
    pub(super) fn of_carrier(carrier: Option<ChannelLane>) -> Self {
        Self::ALL
            .into_iter()
            .find(|lane| {
                lane.class()
                    .is_some_and(|class| Some(class.channel_lane()) == carrier)
            })
            .unwrap_or(Self::Reassembly)
    }

    /// The category an event of this lane that arrived on `carrier` is ordered in: its own
    /// lane's for a frame of known class, and its carrier's for a chunk, whose reassembled
    /// message the lane-mismatch refusal holds to that class; the reassembly lane itself, which
    /// orders every category, for a chunk on no carrier.
    pub(super) fn category(self, carrier: Option<ChannelLane>) -> Self {
        match self {
            Self::Reassembly => Self::of_carrier(carrier),
            lane => lane,
        }
    }

    /// Whether a frame of this lane may have arrived on `carrier`, the transport lane whose
    /// credit it took (one lane per class): a lane of known class only on its class's channel
    /// lane, so the priority lane's credit carries DHT control alone. The reassembly lane's class is
    /// judged once its message is reassembled; no carrier (a frame handed in without a
    /// transport) is judged by nothing.
    pub(in crate::swarm::callback) fn admits_carrier(self, carrier: Option<ChannelLane>) -> bool {
        match (self.class(), carrier) {
            (Some(class), Some(carrier)) => class.channel_lane() == carrier,
            (None, _) | (_, None) => true,
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

#[cfg(test)]
mod tests {
    use rings_transport::core::pool::ChannelLane;

    use super::InboundLane;
    use crate::message::MessageCategory;

    /// A lane of known class admits exactly its class's channel lane; the reassembly lane, and
    /// a frame without a transport carrier, are judged elsewhere (one lane per class).
    #[test]
    fn test_a_lane_admits_only_its_class_channel() {
        let channels = [0, 1, 2, 3].map(ChannelLane::new);
        for lane in InboundLane::ALL {
            for carrier in channels {
                let expected = lane
                    .class()
                    .is_none_or(|class: MessageCategory| class.channel_lane() == carrier);
                assert_eq!(lane.admits_carrier(Some(carrier)), expected, "{lane:?}");
            }
            assert!(lane.admits_carrier(None));
        }
        assert!(!InboundLane::DhtControl.admits_carrier(Some(ChannelLane::new(3))));
        assert!(!InboundLane::Application.admits_carrier(Some(ChannelLane::PRIORITY)));
    }
}
