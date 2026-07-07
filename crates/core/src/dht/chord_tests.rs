//! test module

use std::str::FromStr;

use num_bigint::BigUint;

use super::*;
use crate::ecc::SecretKey;
use crate::tests::default::gen_sorted_dht;

#[tokio::test]
async fn test_chord_finger() -> Result<()> {
    // Setup did a, b, c, d in a clockwise order.
    let a = Did::from_str("0x00E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
    let b = Did::from_str("0x119999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
    let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();
    let d = Did::from_str("0xffffee254729296a45a3885639AC7E10F9d54979").unwrap();

    // This assertion tells you the order of a, b, c, d on the ring.
    // Note that this vec only describes the order, not the absolute position.
    // Since they are all on the ring, you cannot say a is the first element or d is
    // the last. You can only describe their bias based on the same node and a
    // clockwise order.
    //
    // a --> b --> c --> d
    // ^                 |
    // |-----------------|
    //
    let mut seq = vec![a, b, c, d];
    seq.sort();
    assert_eq!(seq, vec![a, b, c, d]);

    // Setup node_a and ensure its successor sequence and finger table is empty.
    let node_a = PeerRing::new_with_storage(a, 3, Box::new(MemStorage::new()));
    assert!(node_a.successors().is_empty()?);
    assert!(node_a.lock_finger()?.is_empty());

    // Test a node won't set itself to successor sequence and finger table.
    assert_eq!(node_a.join(a)?, PeerRingAction::None);
    assert!(node_a.successors().is_empty()?);
    assert!(node_a.lock_finger()?.is_empty());

    // Test join ring with node_b.
    // We don't need to setup node_b here, we just use its did.
    let result = node_a.join(b)?;

    // After join, node_a should ask node_b to find its successor on the ring for
    // connecting.
    assert_eq!(
        result,
        PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(a))
    );

    // This assertion tells you the position of node_b on the ring.
    // Hint: The Did type is a 160-bit unsigned integer.
    assert!(BigUint::from(b) > BigUint::from(2u16).pow(156));
    assert!(BigUint::from(b) < BigUint::from(2u16).pow(157));

    // After join, the finger table of node_a should be like:
    // [b] * 157 + [None] * 3
    let mut expected_finger_list = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected_finger_list.extend(std::iter::repeat_n(None, 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);

    // After join, the successor sequence of node_a should be [b].
    assert_eq!(node_a.successors().list()?, vec![b]);

    // Test repeated join.
    node_a.join(b)?;
    assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
    assert_eq!(node_a.successors().list()?, vec![b]);
    node_a.join(b)?;
    assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
    assert_eq!(node_a.successors().list()?, vec![b]);

    // Test join ring with node_c.
    // We don't need to setup node_c here, we just use its did.
    let result = node_a.join(c)?;

    // Again, after join, node_a should ask node_c to find its successor on the ring
    // for connecting.
    assert_eq!(
        result,
        PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessorForConnect(a))
    );

    // This assertion tells you the position of node_c on the ring.
    // Hint: The Did type is a 160-bit unsigned integer.
    assert!(BigUint::from(c) > BigUint::from(2u16).pow(159));
    assert!(BigUint::from(c) < BigUint::from(2u16).pow(160));

    // After join, the finger table of node_a should be like:
    // [b] * 157 + [c] * 3
    let mut expected_finger_list = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected_finger_list.extend(std::iter::repeat_n(Some(c), 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);

    // After join, the successor sequence of node_a should be [b, c].
    // Because although node_b is closer to node_a, the sequence is not full.
    assert_eq!(node_a.successors().list()?, vec![b, c]);

    // When try to find_successor of node_d, node_a will send query to node_c.
    assert_eq!(
        node_a.find_successor(d).unwrap(),
        PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
    );
    // When try to find_successor of node_c, node_a will send query to node_b.
    assert_eq!(
        node_a.find_successor(c).unwrap(),
        PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(c))
    );

    // Since the test above is clockwise, we need to test anti-clockwise situation.
    let node_a = PeerRing::new_with_storage(a, 3, Box::new(MemStorage::new()));

    // Test join ring with node_c.
    assert_eq!(
        node_a.join(c)?,
        PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessorForConnect(a))
    );
    let expected_finger_list = std::iter::repeat_n(Some(c), 160).collect::<Vec<_>>();
    assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
    assert_eq!(node_a.successors().list()?, vec![c]);

    // Test join ring with node_b.
    assert_eq!(
        node_a.join(b)?,
        PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(a))
    );
    let mut expected_finger_list = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected_finger_list.extend(std::iter::repeat_n(Some(c), 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
    assert_eq!(node_a.successors().list()?, vec![b, c]);

    // Test join over half ring.
    let node_d = PeerRing::new_with_storage(d, 1, Box::new(MemStorage::new()));
    assert_eq!(
        node_d.join(a)?,
        PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessorForConnect(d))
    );

    // This assertion tells you that node_a is over 2^151 far away from node_d.
    // And node_a is also less than 2^152 far away from node_d.
    assert!(d + Did::from(BigUint::from(2u16).pow(151)) < a);
    assert!(d + Did::from(BigUint::from(2u16).pow(152)) > a);

    // After join, the finger table of node_d should be like:
    // [a] * 152 + [None] * 8
    let mut expected_finger_list = std::iter::repeat_n(Some(a), 152).collect::<Vec<_>>();
    expected_finger_list.extend(std::iter::repeat_n(None, 8));
    assert_eq!(node_d.lock_finger()?.list(), &expected_finger_list);

    // After join, the successor sequence of node_a should be [a].
    assert_eq!(node_d.successors().list()?, vec![a]);

    // Test join ring with node_b.
    assert_eq!(
        node_d.join(b)?,
        PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(d))
    );

    // This assertion tells you that node_b is over 2^156 far away from node_d.
    // And node_b is also less than 2^157 far away from node_d.
    assert!(d + Did::from(BigUint::from(2u16).pow(156)) < b);
    assert!(d + Did::from(BigUint::from(2u16).pow(157)) > b);

    // After join, the finger table of node_d should be like:
    // [a] * 152 + [b] * 5 + [None] * 3
    let mut expected_finger_list = std::iter::repeat_n(Some(a), 152).collect::<Vec<_>>();
    expected_finger_list.extend(std::iter::repeat_n(Some(b), 5));
    expected_finger_list.extend(std::iter::repeat_n(None, 3));
    assert_eq!(node_d.lock_finger()?.list(), &expected_finger_list);

    // Note the max successor sequence size of node_d is set to 1 when created.
    // After join, the successor sequence of node_a should still be [a].
    // Because node_a is closer to node_d, and the sequence is full.
    assert_eq!(node_d.successors().list()?, vec![a]);

    Ok(())
}

#[tokio::test]
async fn test_two_node_finger() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() > key2.address() {
        (key1, key2) = (key2, key1)
    }
    let did1: Did = key1.address().into();
    let did2: Did = key2.address().into();
    let node1 = PeerRing::new_with_storage(did1, 3, Box::new(MemStorage::new()));
    let node2 = PeerRing::new_with_storage(did2, 3, Box::new(MemStorage::new()));

    node1.join(did2)?;
    node2.join(did1)?;
    assert!(node1.successors().list()?.contains(&did2));
    assert!(node2.successors().list()?.contains(&did1));

    assert!(
        node1.lock_finger()?.contains(Some(did2)),
        "did1:{did1:?}; did2:{did2:?}"
    );
    assert!(
        node2.lock_finger()?.contains(Some(did1)),
        "did1:{did1:?}; did2:{did2:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_two_node_finger_failed_case() -> Result<()> {
    let did1 = Did::from_str("0x051cf4f8d020cb910474bef3e17f153fface2b5f").unwrap();
    let did2 = Did::from_str("0x54baa7dc9e28f41da5d71af8fa6f2a302be1c1bf").unwrap();
    let max = Did::from(BigUint::from(2u16).pow(160) - 1u16);
    let zero = Did::from(BigUint::from(2u16).pow(160));

    let node1 = PeerRing::new_with_storage(did1, 3, Box::new(MemStorage::new()));
    let node2 = PeerRing::new_with_storage(did2, 3, Box::new(MemStorage::new()));

    node1.join(did2)?;
    node2.join(did1)?;
    assert!(node1.successors().list()?.contains(&did2));
    assert!(node2.successors().list()?.contains(&did1));
    let pos_159 = did2 + Did::from(BigUint::from(2u16).pow(159));
    assert!(pos_159 > did2);
    assert!(pos_159 < max, "{pos_159:?};{max:?}");
    let pos_160 = did2 + zero;
    assert_eq!(pos_160, did2);
    assert!(pos_160 > did1);

    assert!(
        node1.lock_finger()?.contains(Some(did2)),
        "did1:{did1:?}; did2:{did2:?}"
    );
    assert!(
        node2.lock_finger()?.contains(Some(did1)),
        "did2:{did2:?} dont contains did1:{did1:?}"
    );

    Ok(())
}

#[test]
fn test_correct_chord_stabilize_handles_empty_successor_info() -> Result<()> {
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

/// Test Correct Chord implementation
#[tokio::test]
async fn test_correct_chord_impl() -> Result<()> {
    fn assert_successor(dht: &PeerRing, did: &Did) -> bool {
        let succ_list = dht.successors();
        succ_list.list().unwrap().contains(did)
    }

    /// check that two dht is mutual successors
    fn check_is_mutual_successors(dht1: &PeerRing, dht2: &PeerRing) {
        let succ_list_1 = dht1.successors();
        let succ_list_2 = dht2.successors();
        assert_eq!(succ_list_1.min().unwrap(), dht2.did);
        assert_eq!(succ_list_2.min().unwrap(), dht1.did);
    }

    fn check_succ_is_including(dht: &PeerRing, dids: Vec<Did>) {
        let succ_list = dht.successors();
        for did in dids {
            assert!(succ_list.list().unwrap().contains(&did));
        }
    }

    let dhts = gen_sorted_dht(5);
    let [n1, n2, n3, n4, n5] = dhts.as_slice() else {
        panic!("wrong dhts length");
    };
    // we now have:
    // n1 < n2 < n3 < n4

    // n1 join n2
    n1.join(n2.did).unwrap();
    n2.join(n1.did).unwrap();
    // for now n1, n2 are `mutual successors`.
    check_is_mutual_successors(n1, n2);
    // n1 join n3

    n1.join(n3.did).unwrap();
    n1.join(n4.did).unwrap();
    // for now n1's successor should include n1 and n3
    check_succ_is_including(n1, vec![n2.did, n3.did, n4.did]);

    n1.join(n5.did).unwrap();
    // n5 is not in n1's successor list
    assert!(!assert_successor(n1, &n5.did));

    #[allow(non_local_definitions)]
    #[cfg_attr(feature = "wasm", async_trait(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_trait)]
    impl LiveDid for Did {
        async fn live(&self) -> bool {
            true
        }
    }

    if let PeerRingAction::MultiActions(rets) = n5.join_then_sync(n1.did).await.unwrap() {
        for r in rets {
            if let PeerRingAction::RemoteAction(t, _) = r {
                assert_eq!(t, n1.did)
            } else {
                panic!("wrong remote");
            }
        }
    } else {
        panic!("Wrong ret");
    }
    Ok(())
}
