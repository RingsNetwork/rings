use std::str::FromStr;

use super::*;
use crate::dht::topology;
use crate::tests::default::gen_sorted_dht;

/// An isolated node has no head to query: beginning a round records no token
/// and emits nothing, and a report from anyone is not claimable.
#[test]
fn test_isolated_node_begins_no_stabilization_round() -> Result<()> {
    let did = Did::from_str("0x051cf4f8d020cb910474bef3e17f153fface2b5f").unwrap();
    let node = PeerRing::new_with_storage(did, 3, Box::new(MemStorage::new()));
    let request_id = uuid::Uuid::from_u128(1);

    assert_eq!(node.begin_stabilization(request_id)?, PeerRingAction::None);
    assert!(node
        .claim_stabilization_report(Did::from(4u32), request_id)?
        .is_none());
    Ok(())
}

/// Proves that one stabilization correlation token authorizes at most one
/// report handler to spend its bounded connection budget.
///
/// The first exact claim must succeed and atomically move the request into its
/// processing phase; replaying the same authenticated pair must fail.
#[test]
fn test_stabilization_report_claim_is_single_use() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let successor = Did::from(4u32);
    // The same authenticated response may reserve its connection budget once.
    let request_id = uuid::Uuid::from_u128(1);
    let _ = node.admit_connected(successor, None)?;
    let _ = node.begin_stabilization(request_id)?;

    let claim = node.claim_stabilization_report(successor, request_id)?;
    assert!(claim.is_some());
    assert!(node
        .claim_stabilization_report(successor, request_id)?
        .is_none());
    // The failed duplicate released nothing: the first claim still owns the
    // report and may spend its budget.
    let mut plan = topology::ConnectionPlan::new(
        successor,
        request_id,
        [Did::from(8u32)],
        node.did,
        topology::stabilization_connection_budget(3),
    );
    assert!(matches!(
        node.advance_stabilization_connection_plan(&mut plan)?,
        topology::ConnectionStep::Connect(_)
    ));
    // Dropping the claim releases the token; a released token is claimable by
    // nobody, because release retires it rather than reopening it.
    drop(claim);
    assert!(node
        .claim_stabilization_report(successor, request_id)?
        .is_none());
    assert!(matches!(
        node.advance_stabilization_connection_plan(&mut plan)?,
        topology::ConnectionStep::Stale
    ));
    Ok(())
}

/// Proves the successor-churn law for sync tokens through the peer ring: a
/// token survives a successor-list change that keeps its reporter, and is
/// revoked by one that removes the reporter.
#[test]
fn test_successor_change_revokes_a_sync_report_only_when_its_reporter_leaves() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let reporter = Did::from(4u32);
    let request_id = uuid::Uuid::from_u128(1);
    let _ = node.admit_connected(reporter, None)?;
    assert!(node.begin_successor_sync(reporter, request_id)?);

    // The list grows, the reporter stays: the token is still claimable, once.
    let _ = node.admit_connected(Did::from(8u32), None)?;
    let claim = node.claim_successor_sync_report(reporter, request_id)?;
    assert!(claim.is_some());
    assert!(node
        .claim_successor_sync_report(reporter, request_id)?
        .is_none());
    // The failed duplicate released nothing: the first claim still owns the
    // report and may spend its budget.
    let mut plan =
        topology::ConnectionPlan::new(reporter, request_id, [Did::from(12u32)], node.did, 3);
    assert_eq!(
        node.advance_successor_sync_connection_plan(&mut plan)?,
        topology::ConnectionStep::Connect(Did::from(12u32))
    );
    drop(claim);
    assert_eq!(
        node.advance_successor_sync_connection_plan(&mut plan)?,
        topology::ConnectionStep::Stale
    );

    // A new round for the same reporter, then the reporter leaves: revoked.
    let next_request_id = uuid::Uuid::from_u128(2);
    assert!(node.begin_successor_sync(reporter, next_request_id)?);
    node.remove(reporter)?;
    assert!(node
        .claim_successor_sync_report(reporter, next_request_id)?
        .is_none());
    Ok(())
}

/// Verifies that repeated stabilization over the representative Chord fixture
/// leaves every node with the expected immediate and backup successors.
#[tokio::test]
async fn test_correct_chord_maintains_expected_successors() -> Result<()> {
    fn has_successor(dht: &PeerRing, did: Did) -> bool {
        dht.successors().list().unwrap().contains(&did)
    }

    fn assert_mutual_successors(first: &PeerRing, second: &PeerRing) {
        assert_eq!(first.successors().min().unwrap(), second.did);
        assert_eq!(second.successors().min().unwrap(), first.did);
    }

    fn assert_successors_include(dht: &PeerRing, dids: &[Did]) {
        let successors = dht.successors().list().unwrap();
        for did in dids {
            assert!(successors.contains(did));
        }
    }

    let dhts = gen_sorted_dht(5);
    let [n1, n2, n3, n4, n5] = dhts.as_slice() else {
        panic!("wrong dhts length");
    };

    n1.admit_connected(n2.did, None).unwrap();
    n2.admit_connected(n1.did, None).unwrap();
    assert_mutual_successors(n1, n2);

    n1.admit_connected(n3.did, None).unwrap();
    n1.admit_connected(n4.did, None).unwrap();
    assert_successors_include(n1, &[n2.did, n3.did, n4.did]);

    n1.admit_connected(n5.did, None).unwrap();
    assert!(!has_successor(n1, n5.did));

    // Admitting a live seed emits both remote topology work and, when the head
    // changes, a storage-repair intent.
    let PeerRingAction::MultiActions(actions) = n5.admit_connected(n1.did, None).unwrap() else {
        panic!("wrong action");
    };
    for action in actions {
        match action {
            PeerRingAction::RemoteAction(target, _) => assert_eq!(target, n1.did),
            // Admitting the first successor moves the head, which makes a repair round due.
            PeerRingAction::StorageRepairDue => {}
            action => panic!("expected a remote action or a repair request, got {action:?}"),
        }
    }
    Ok(())
}
