//! Registration readiness over a network (#866), in production order. A node listens before it
//! has an edge, so its first heartbeat settles at itself alone, which makes nothing ready. Its
//! first edge publishes the registration again (#855); the registry's owner reports that
//! publish stored, and `registrations_published` resolves on the report. One directory read at
//! the owner then lists the node, with no probe.
//!
//! Every wait is woken by an event (activity, or the report); the hang guard bounds only a hang.

use futures::FutureExt;

use super::common::*;
use super::*;
use crate::consts::DATA_REDUNDANT;
use crate::test_support::TestTask;

/// `registrations_published` resolves on the owner's report of the first edge's publish, never
/// on a publish made alone, and a single read at the owner lists the node after it.
#[tokio::test]
async fn test_registrations_published_resolves_on_the_owners_report() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (publisher, owner) = prepare_online_node_registry_pair(42).await?;
    let publisher = Arc::new(publisher);
    let _recorder = ApplicationRecorder::attach(&publisher);
    let _owner_recorder = ApplicationRecorder::attach(&owner);
    let registry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
    let placement_keys = registry_key.rotate_affine(DATA_REDUNDANT)?;
    let expected = BTreeSet::from([publisher.did()]);

    let listening = Arc::clone(&publisher);
    let _listener = TestTask::spawn(async move { listening.listen().await });
    wait_for_online_node_dids_in_storage(
        &publisher,
        placement_keys.as_slice(),
        &expected,
        "the first heartbeat, alone",
    )
    .await?;
    assert!(
        publisher.registrations_published().now_or_never().is_none(),
        "a publish made alone reached no owner"
    );

    connect_processors(&publisher, &owner).await;
    tokio::time::timeout(
        PROCESSOR_TEST_HANG_GUARD,
        publisher.registrations_published(),
    )
    .await
    .map_err(|_| Error::InvalidConfig("no store report within the hang guard".to_string()))?;

    let listed = owner
        .lookup_online_nodes(false)
        .await?
        .into_iter()
        .map(|descriptor| descriptor.did)
        .collect::<BTreeSet<_>>();
    assert!(
        expected.is_subset(&listed),
        "one read at the owner lists the node"
    );
    Ok(())
}
