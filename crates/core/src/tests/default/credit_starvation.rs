//! Starving a link of credit, for witnesses of the laws that hold while a peer withholds it.

use rings_transport::connections::dummy_controlled;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::pool::DATA_CHANNEL_POOL_SIZE;
use rings_transport::core::transport::LaneCreditReservation;

use super::prepare_node;
use super::wait_for_msgs;
use super::wait_for_successor;
use super::Node;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::tests::manually_establish_connection;

/// Two nodes connected to each other, settled.
pub(crate) async fn connected_pair() -> Result<(Node, Node)> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    Ok((node1, node2))
}

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
