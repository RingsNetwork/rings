use std::str::FromStr;

use async_trait::async_trait;

use super::*;
use crate::dht::LiveDid;
use crate::tests::default::gen_sorted_dht;

#[test]
fn test_stabilize_handles_empty_successor_info() -> Result<()> {
    let did = Did::from_str("0x051cf4f8d020cb910474bef3e17f153fface2b5f").unwrap();
    let node = PeerRing::new_with_storage(did, 3, Box::new(MemStorage::new()));

    assert_eq!(
        node.stabilize(TopoInfo {
            successors: vec![],
            predecessor: None,
        })?,
        PeerRingAction::MultiActions(vec![])
    );
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
    let _ = node.join(successor)?;
    let _ = node.begin_stabilization(request_id)?;

    assert!(node.claim_stabilization_report(successor, request_id)?);
    assert!(!node.claim_stabilization_report(successor, request_id)?);
    Ok(())
}

/// Proves that successor-list churn revokes reports registered against the
/// previous successor snapshot.
///
/// The test registers a sync request, changes the successor set, and verifies
/// that the old reporter/token pair can no longer be claimed.
#[test]
fn test_successor_change_invalidates_an_outstanding_sync_report() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let reporter = Did::from(4u32);
    // Successor-sync reports are tied to the exact successor list observed when
    // the query was sent.
    let request_id = uuid::Uuid::from_u128(1);
    let _ = node.join(reporter)?;
    assert!(node.begin_successor_sync(reporter, request_id)?);

    let _ = node.join(Did::from(8u32))?;

    assert!(!node.claim_successor_sync_report(reporter, request_id)?);
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

    n1.join(n2.did).unwrap();
    n2.join(n1.did).unwrap();
    assert_mutual_successors(n1, n2);

    n1.join(n3.did).unwrap();
    n1.join(n4.did).unwrap();
    assert_successors_include(n1, &[n2.did, n3.did, n4.did]);

    n1.join(n5.did).unwrap();
    assert!(!has_successor(n1, n5.did));

    #[allow(non_local_definitions)]
    #[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
    #[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
    impl LiveDid for Did {
        async fn live(&self) -> bool {
            true
        }
    }

    // Joining through an already live seed emits both remote topology work and,
    // when the head changes, a storage-repair intent.
    let PeerRingAction::MultiActions(actions) = n5.join_then_sync(n1.did).await.unwrap() else {
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
