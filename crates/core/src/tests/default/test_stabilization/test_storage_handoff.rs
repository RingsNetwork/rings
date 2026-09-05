//! Placement invariant of the storage repair pass: every live local entry lies in `(self, head]`,
//! or the pass offers it to the head as an ownership hand-off. The invariant is over the ring
//! state, so it holds whichever input moved the head; a direct connection is the case no message
//! reports, and admitting it only requests the pass.

use super::*;
use crate::dht::STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS;
use crate::ecc::tests::gen_ordered_keys;
use crate::message::Encoder;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_storage_absence;
use crate::tests::default::wait_for_storage_entry;
use crate::tests::live_entry;

/// After a direct connection moves the successor head, admission requests a repair pass, and the
/// owner's pass hands the entries placed beyond the new head over to it once the connection has
/// outlived the fresh-connection grace. No notify report is involved, and the local copy is
/// removed only by the receiver's acknowledgement.
#[tokio::test]
async fn test_repair_pass_hands_off_entries_beyond_a_directly_connected_head() -> Result<()> {
    let [key1, key2, key3] = gen_ordered_keys::<3>();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    let node3 = prepare_node(key3).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node3]).await;
    wait_for_successor(&node1, node3.did()).await?;

    // `node3 ∈ (node1, node3]`, so node1 owns the key while node3 is its head.
    let entry = live_entry(
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
        PeerRingAction::Some(witness) if witness == node3.did()
    ));

    // node2 connects straight to node1. Admission moves node1's head to node2, so the key now
    // lies beyond `(node1, node2]`; the head change requests a repair pass rather than sending.
    manually_establish_connection(&node2.swarm, &node1.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;
    wait_for_successor(&node1, node2.did()).await?;
    assert!(node1.swarm.transport.storage_repair_requested());

    // The pass defers to a connection younger than the grace, which is what outlives the peer's
    // own admission of this node; age the connection past it, then run the requested pass.
    node1.swarm.transport.force_peer_connected_at(
        node2.did(),
        get_epoch_ms_i64() - STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS - 1,
    )?;
    assert_eq!(
        node1
            .swarm
            .stabilizer()
            .run_requested_storage_maintenance()
            .await,
        Some(StorageRepairOutcome::Complete)
    );

    assert_eq!(
        wait_for_storage_entry(&node2, entry.did).await?,
        stored_entry
    );
    wait_for_storage_absence(&node1, entry.did).await?;
    Ok(())
}
