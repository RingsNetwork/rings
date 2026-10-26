//! The first-edge law of registration (#855) over a network, in production order: a node that
//! listens before it has any edge publishes its first registration at itself alone, and its first
//! admitted edge publishes it again, before its heartbeat, so the registry's owner stores a newer
//! write of it.
//!
//! The witness is the dot: the early publish re-appends the descriptor under a fresh, higher dot
//! (the writer-order law of `rings_core::dht`), while the storage repair that hands the alone
//! copy to the owner at the head change keeps its dot, and the next heartbeat (30 s) outlasts the
//! hang guard (5 s on the controlled network). Every wait is woken by activity; the guard only
//! bounds a hang.

use rings_core::dht::entry::EntryKind;
use rings_core::dht::entry::EntryVersion;

use super::common::*;
use super::*;
use crate::consts::DATA_REDUNDANT;
use crate::descriptor::RegistryElement;
use crate::online::OnlineNodeDescriptor;
use crate::test_support::TestTask;
use crate::tests::activity::probe_on_activity;

/// The dot of `publisher`'s newest descriptor stored at every one of `placement_keys` in
/// `processor`'s storage: the least of the placements' newest, `None` while one holds none.
async fn stored_dot(
    processor: &Processor,
    placement_keys: &[Did],
    publisher: Did,
) -> Result<Option<EntryVersion>> {
    let mut least: Option<EntryVersion> = None;
    for placement_key in placement_keys {
        let entry = processor
            .swarm
            .dht()
            .stored_entry_at(EntryKind::Data, *placement_key, get_epoch_ms())
            .await
            .map_err(Error::Storage)?;
        let newest = entry.and_then(|entry| {
            entry
                .data
                .iter()
                .zip(entry.crdt.dots.iter())
                .filter(|(element, _)| {
                    OnlineNodeDescriptor::from_element(element)
                        .is_ok_and(|descriptor| descriptor.did == publisher)
                })
                .map(|(_, dot)| dot.version)
                .max()
        });
        let Some(newest) = newest else {
            return Ok(None);
        };
        least = Some(least.map_or(newest, |least| least.min(newest)));
    }
    Ok(least)
}

/// A registration made before any edge reaches its owner at the first edge as a newer write, not
/// a heartbeat later.
#[tokio::test]
async fn test_a_registration_made_alone_is_published_again_at_the_first_edge() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (publisher, owner) = prepare_online_node_registry_pair(42).await?;
    let publisher = Arc::new(publisher);
    let _recorder = ApplicationRecorder::attach(&publisher);
    let _owner_recorder = ApplicationRecorder::attach(&owner);
    let registry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    let placement_keys = registry_key.rotate_affine(DATA_REDUNDANT)?;
    let placement_keys = placement_keys.as_slice();
    let did = publisher.did();

    let listening = Arc::clone(&publisher);
    let _listener = TestTask::spawn(async move { listening.listen().await });
    // Alone, the publisher owns every placement: its first heartbeat settles there only.
    let alone = probe_on_activity(
        "the first heartbeat, alone",
        PROCESSOR_TEST_HANG_GUARD,
        || stored_dot(&publisher, placement_keys, did),
    )
    .await?;
    assert_eq!(publisher.swarm.admitted_edges(), 0);

    connect_processors(&publisher, &owner).await;

    probe_on_activity(
        "the owner stores the early publish of the first edge",
        PROCESSOR_TEST_HANG_GUARD,
        || async {
            let stored = stored_dot(&owner, placement_keys, did).await?;
            Ok(stored.filter(|dot| *dot > alone).map(drop))
        },
    )
    .await
}
