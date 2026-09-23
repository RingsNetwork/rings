//! Production composition after a bounded departure and rejoin. One successor
//! slot makes the returning peer visible only as the queried head's predecessor;
//! zero finger slots exclude an unrelated lookup from discovering that peer.

use std::sync::Arc;

use rings_transport::core::transport::WebrtcConnectionState;

use super::*;
use crate::ecc::SecretKey;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::SwarmBuilder;

/// Build a real swarm whose topology reports separate predecessor discovery
/// from successor-list discovery. Storage and message handling remain unchanged.
fn head_only_node(key: SecretKey) -> Result<Node> {
    // The session authenticates this node's real connection and topology messages.
    let session = SessionSk::new_with_seckey(&key)?;
    // Only topology breadth is reduced; the production transport and storage remain enabled.
    let swarm = SwarmBuilder::new(
        crate::tests::TEST_NETWORK_ID,
        "stun://stun.l.google.com:19302",
        Box::new(MemStorage::new()),
        session,
    )
    .dht_succ_max(1)
    .dht_finger_table_size(0)
    .dht_virtual_nodes(0)
    .build();
    Ok(Node::new(Arc::new(swarm)))
}

/// Retire the observer's head through the production unavailable-peer sweep.
/// The surviving admitted link must supply a new head: ordinary removal of
/// the only successor would instead enter the orphan case owned by #775.
async fn retire_head(observer: &Node, departed: &Node, surviving: &Node) -> Result<()> {
    observer
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            departed.did(),
            WebrtcConnectionState::Closed,
        )?;
    observer
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;
    departed.swarm.disconnect(observer.did()).await?;
    wait_for_msgs([observer, departed, surviving]).await;
    assert!(!observer
        .swarm
        .transport
        .has_active_connection(departed.did()));
    assert!(!observer.dht().topology_state()?.references(departed.did()));
    assert_eq!(observer.dht().successors().list()?, vec![surviving.did()]);
    Ok(())
}

/// A departed peer rejoins through its successor. Once its link to the inbox
/// owner is absent, the owner's real topology query must discover the closer
/// predecessor, admit a newer connection, notify that selected head, and hand
/// over the held message. Every transition runs through the production shell;
/// this checks the stated finite schedule, not arbitrary churn recovery.
#[tokio::test]
async fn test_topology_predecessor_discovery_after_rejoin_delivers_held_inbox() -> Result<()> {
    // Ring order is fixed by the generated ordered identities: owner < peer < head.
    let [owner_key, peer_key, head_key] = gen_ordered_keys::<3>();
    let owner = head_only_node(owner_key)?;
    let peer = head_only_node(peer_key)?;
    let head = head_only_node(head_key)?;
    manually_establish_connection(&owner.swarm, &head.swarm).await;
    manually_establish_connection(&peer.swarm, &head.swarm).await;
    wait_for_msgs([&owner, &peer, &head]).await;
    for node in [&owner, &peer, &head] {
        node.swarm.stabilizer().stabilize().await?;
        wait_for_msgs([&owner, &peer, &head]).await;
    }
    wait_for_predecessor(&head, peer.did()).await?;
    wait_for_successor(&owner, peer.did()).await?;

    // The peer leaves completely while the two survivors retain a live ring.
    retire_head(&owner, &peer, &head).await?;
    peer.swarm.disconnect(head.did()).await?;
    head.swarm.disconnect(peer.did()).await?;
    wait_for_msgs([&owner, &peer, &head]).await;
    assert!(peer.swarm.peers().is_empty());
    hold_message_for_offline_peer(&owner, &head, peer.did()).await?;

    // Admission's successor synchronization may eagerly reconnect owner and peer.
    // Remove that link before the measured round so it cannot satisfy discovery.
    manually_establish_connection(&peer.swarm, &head.swarm).await;
    wait_for_msgs([&owner, &peer, &head]).await;
    // Preserve the generation being retired as the later re-admission witness.
    let retired = owner
        .swarm
        .transport
        .active_attempt(peer.did())?
        .ok_or(Error::SwarmMissTransport(peer.did()))?;
    retire_head(&owner, &peer, &head).await?;

    // Negative control: before peer notifies head, the report contains only
    // owner itself. Neither successor synchronization nor fingers can discover peer.
    assert_eq!(*head.dht().lock_predecessor()?, Some(owner.did()));
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([&owner, &peer, &head]).await;
    assert!(!owner.swarm.transport.has_active_connection(peer.did()));
    assert_eq!(owner.dht().successors().list()?, vec![head.did()]);

    // Deliver a real authenticated notify to peer's selected head, without
    // starting an unrelated successor-sync or finger-discovery round at peer.
    peer.swarm
        .send_message(
            Message::NotifyPredecessorSend(crate::message::NotifyPredecessorSend {
                did: peer.did(),
            }),
            head.did(),
        )
        .await?;
    wait_for_predecessor(&head, peer.did()).await?;

    // The only report field that can reveal peer is the closer predecessor.
    assert_eq!(head.dht().successors().list()?, vec![owner.did()]);
    assert_eq!(*head.dht().lock_predecessor()?, Some(peer.did()));
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_msgs([&owner, &peer, &head]).await;
    // A new generation proves discovery admitted a replacement rather than reusing the old link.
    let admitted = owner
        .swarm
        .transport
        .active_attempt(peer.did())?
        .ok_or(Error::SwarmMissTransport(peer.did()))?;
    assert!(admitted.generation() > retired.generation());
    assert_eq!(owner.dht().successors().list()?, vec![peer.did()]);
    // Connecting a candidate can finish after the report was filtered. The
    // following completed round must notify the newly admitted head.
    owner.swarm.stabilizer().stabilize().await?;
    wait_for_predecessor(&peer, owner.did()).await?;

    // Completed notifications restore the three-node head/predecessor cycle.
    assert_eq!(peer.dht().successors().list()?, vec![head.did()]);
    assert_eq!(*owner.dht().lock_predecessor()?, Some(head.did()));
    assert_eq!(*head.dht().lock_predecessor()?, Some(peer.did()));
    hand_off_inbox(&owner, &peer).await?;
    drain_and_assert_delivered(&peer, owner.did()).await
}
