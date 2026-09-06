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
