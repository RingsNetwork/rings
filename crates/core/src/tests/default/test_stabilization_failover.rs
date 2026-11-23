use rings_transport::core::transport::TransportConnectionState;

use super::test_stabilization::replace_observed_topology;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

async fn assert_unavailable_successor_fails_over(
    state: TransportConnectionState,
    channel_open: Option<bool>,
) -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    let node3 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;

    replace_observed_topology(&node1, &[node2.did()], Some(node2.did()), &[
        (0, node3.did()),
        (3, node3.did()),
    ])?;

    node1
        .swarm
        .transport
        .force_connection_state_without_callback(node2.did(), state)?;
    if let Some(open) = channel_open {
        node1
            .swarm
            .transport
            .force_peer_channel_open_without_callback(node2.did(), Some(open))?;
    }

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(!node1.swarm.transport.has_active_connection(node2.did()));
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(!node1.dht().successors().contains(&node2.did())?);
    assert!(node1.dht().successors().contains(&node3.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, None);
    assert!(!node1.dht().lock_finger()?.contains(Some(node2.did())));
    assert!(node1.dht().lock_finger()?.contains(Some(node3.did())));
    assert!(node1.swarm.transport.has_active_connection(node3.did()));
    assert!(node1.swarm.transport.storage_repair_requested());
    assert_eq!(
        node1
            .swarm
            .transport
            .get_connection(node3.did())
            .map(|conn| conn.connection_state()),
        Some(TransportConnectionState::Connected)
    );

    Ok(())
}

#[tokio::test]
async fn test_unavailable_successor_states_only_promote_a_live_replacement() -> Result<()> {
    for (state, channel_open) in [
        (TransportConnectionState::Disconnected, None),
        (TransportConnectionState::Failed, None),
        (TransportConnectionState::Closed, None),
        (TransportConnectionState::Connected, Some(false)),
    ] {
        assert_unavailable_successor_fails_over(state, channel_open).await?;
    }
    Ok(())
}
