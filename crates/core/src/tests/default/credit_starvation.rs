//! Starving a link of credit, for witnesses of the laws that hold while a peer withholds it.

use rings_transport::connections::dummy_controlled;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::pool::DATA_CHANNEL_POOL_SIZE;
use rings_transport::core::transport::LaneCreditReservation;

use super::Node;
use crate::error::Error;
use crate::error::Result;

/// Make `withholder` grant `peer` no more credit, then hold at `peer` every credit of `lane`
/// already granted: the lane is starved for good, with no grant still to come.
pub(crate) fn starve(
    withholder: &Node,
    peer: &Node,
    lane: ChannelLane,
) -> Result<Vec<LaneCreditReservation>> {
    let connection = withholder
        .swarm
        .transport
        .get_connection(peer.did())
        .ok_or(Error::SwarmMissDidInTable(peer.did()))?;
    dummy_controlled::withhold_credit(&connection.dummy_generation_id()?);
    peer.swarm
        .transport
        .hold_lane_credit_for_test(withholder.did(), lane)
}

/// [`starve`] on every lane of the link.
pub(crate) fn starve_every_lane(
    withholder: &Node,
    peer: &Node,
) -> Result<Vec<LaneCreditReservation>> {
    let mut held = Vec::new();
    for lane in 0..DATA_CHANNEL_POOL_SIZE {
        held.extend(starve(withholder, peer, ChannelLane::new(lane))?);
    }
    Ok(held)
}
