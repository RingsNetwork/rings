use rings_transport::core::pool::ChannelLane;

pub(in crate::swarm::transport) use crate::message::MessageCategory as TransferClass;
pub(in crate::swarm::transport) use crate::message::MessageKind as OutboundMessageKind;

/// The connection lane a class travels on: one lane per class (#906), so the transport keeps a
/// class's frames on one ordered data channel.
///
/// Law: a class's frames reach the peer's handler in the order the class lane admitted them,
/// whatever the other classes' channels do. With at most `OUTBOUND_LANE_WINDOW` of them in
/// flight, fewer than the replay window, a class stream never arrives reordered beyond it.
pub(in crate::swarm::transport) const fn channel_lane(class: TransferClass) -> ChannelLane {
    match class {
        TransferClass::DhtControl => ChannelLane::new(0),
        TransferClass::Storage => ChannelLane::new(1),
        TransferClass::E2e => ChannelLane::new(2),
        TransferClass::Application => ChannelLane::new(3),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum OutboundCompletion {
    Detached,
    Tracked,
}
