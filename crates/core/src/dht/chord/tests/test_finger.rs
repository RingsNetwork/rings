use std::str::FromStr;

use num_bigint::BigUint;

use super::*;

/// The actions of a join that makes `peer` the successor head: the connect lookup and, by the
/// topology head law, the hand-off request toward the new head.
fn connect_and_hand_off(peer: Did, local: Did) -> PeerRingAction {
    PeerRingAction::MultiActions(vec![
        PeerRingAction::RemoteAction(peer, RemoteAction::FindSuccessorForConnect(local)),
        PeerRingAction::RemoteAction(peer, RemoteAction::HandOffStorage),
    ])
}

#[tokio::test]
async fn test_finger_table_tracks_clockwise_and_wrapped_joins() -> Result<()> {
    let a = Did::from_str("0x00E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
    let b = Did::from_str("0x119999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
    let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();
    let d = Did::from_str("0xffffee254729296a45a3885639AC7E10F9d54979").unwrap();

    let mut sequence = vec![a, b, c, d];
    sequence.sort();
    assert_eq!(sequence, vec![a, b, c, d]);

    let node_a = PeerRing::new_with_storage(a, 3, Box::new(MemStorage::new()));
    assert!(node_a.successors().is_empty()?);
    assert!(node_a.lock_finger()?.is_empty());

    assert_eq!(node_a.join(a)?, PeerRingAction::None);
    assert!(node_a.successors().is_empty()?);
    assert!(node_a.lock_finger()?.is_empty());

    assert_eq!(node_a.join(b)?, connect_and_hand_off(b, a));
    assert!(BigUint::from(b) > BigUint::from(2u16).pow(156));
    assert!(BigUint::from(b) < BigUint::from(2u16).pow(157));

    let mut expected = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(None, 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![b]);

    for _ in 0..2 {
        node_a.join(b)?;
        assert_eq!(node_a.lock_finger()?.list(), &expected);
        assert_eq!(node_a.successors().list()?, vec![b]);
    }

    assert_eq!(
        node_a.join(c)?,
        PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessorForConnect(a))
    );
    assert!(BigUint::from(c) > BigUint::from(2u16).pow(159));
    assert!(BigUint::from(c) < BigUint::from(2u16).pow(160));

    let mut expected = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(Some(c), 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![b, c]);
    assert_eq!(
        node_a.find_successor(d)?,
        PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
    );
    assert_eq!(
        node_a.find_successor(c)?,
        PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(c))
    );

    let node_a = PeerRing::new_with_storage(a, 3, Box::new(MemStorage::new()));
    assert_eq!(node_a.join(c)?, connect_and_hand_off(c, a));
    let expected = std::iter::repeat_n(Some(c), 160).collect::<Vec<_>>();
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![c]);

    assert_eq!(node_a.join(b)?, connect_and_hand_off(b, a));
    let mut expected = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(Some(c), 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![b, c]);

    let node_d = PeerRing::new_with_storage(d, 1, Box::new(MemStorage::new()));
    assert_eq!(node_d.join(a)?, connect_and_hand_off(a, d));
    assert!(d + Did::from(BigUint::from(2u16).pow(151)) < a);
    assert!(d + Did::from(BigUint::from(2u16).pow(152)) > a);

    let mut expected = std::iter::repeat_n(Some(a), 152).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(None, 8));
    assert_eq!(node_d.lock_finger()?.list(), &expected);
    assert_eq!(node_d.successors().list()?, vec![a]);

    assert_eq!(
        node_d.join(b)?,
        PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(d))
    );
    assert!(d + Did::from(BigUint::from(2u16).pow(156)) < b);
    assert!(d + Did::from(BigUint::from(2u16).pow(157)) > b);

    let mut expected = std::iter::repeat_n(Some(a), 152).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(Some(b), 5));
    expected.extend(std::iter::repeat_n(None, 3));
    assert_eq!(node_d.lock_finger()?.list(), &expected);
    assert_eq!(node_d.successors().list()?, vec![a]);
    Ok(())
}
