//! Placement invariant of the stabilizer: every live local entry lies in `(self, head]`, or the
//! round offers it to the head as an ownership hand-off. The invariant is over the ring state, so
//! it holds whichever input moved the head; a direct connection is the case no message reports.

use super::*;
use crate::ecc::tests::gen_ordered_keys;
use crate::message::Encoder;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_storage_absence;
use crate::tests::default::wait_for_storage_entry;

/// Placement invariant at the stabilizer boundary: after a direct connection moves the successor
/// head, the owner's next round hands the entries placed beyond the new head over to it. No notify
/// report is involved, and the local copy is removed only by the receiver's acknowledgement.
#[tokio::test]
async fn test_owner_round_hands_off_entries_beyond_a_directly_connected_head() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node3]).await;
    wait_for_successor(&node1, node3.did()).await?;

    // `node3 ∈ (node1, node3]`, so node1 owns the key while node3 is its head.
    let entry = crate::tests::live_entry(
        node3.did(),
        vec![String::from("sync me").encode()?],
        EntryKind::Data,
    );
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;
    let stored_entry = entry.clone().try_into_storage_entry()?;
    assert!(matches!(
        node1.dht().find_storage_owner(entry.did)?,
        PeerRingAction::Some(_)
    ));

    // node2 connects straight to node1. Admission moves node1's head to node2, so the key now
    // lies beyond `(node1, node2]` and node1's next round hands it over.
    manually_establish_connection(&node2.swarm, &node1.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node1, node2.did()).await?;
    node1.swarm.stabilizer()?.stabilize().await?;

    assert_eq!(
        wait_for_storage_entry(&node2, entry.did).await?,
        stored_entry
    );
    wait_for_storage_absence(&node1, entry.did).await?;
    Ok(())
}
