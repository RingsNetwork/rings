use std::str::FromStr;

use num_bigint::BigUint;

use super::*;

/// The actions of a join that makes `peer` the successor head: the connect lookup and, by the
/// topology head law, the hand-off request toward the new head.
fn connect_and_hand_off(peer: Did, local: Did) -> PeerRingAction {
    PeerRingAction::MultiActions(vec![
        PeerRingAction::RemoteAction(peer, RemoteAction::FindSuccessorForConnect(local)),
        PeerRingAction::StorageRepairDue,
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

    // `b` covers slots below its highest set bit; larger slots remain unknown.
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

    // `c` is farther away, so it only refines the high sparse/no-wrap slots.
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

#[test]
fn test_public_fix_fingers_advances_one_range() -> Result<()> {
    // This public API regression protects the legacy `fix_fingers()` contract: a caller that asks
    // for one repair pass should both begin revalidation and issue the first routable lookup, not
    // only mark the table stale for a later maintenance tick.
    let local = Did::from(0u32);
    // The seed is the only remote evidence, so the first public fix routes to it.
    let seed = Did::from(8u32);
    let dht = PeerRing::new_with_storage(local, 3, Box::new(MemStorage::new()));
    let _ = dht.join(seed)?;

    assert!(matches!(
        dht.fix_fingers()?,
        PeerRingAction::RemoteAction(
            next,
            RemoteAction::FindSuccessorForFix { .. }
        ) if next == seed
    ));
    Ok(())
}

#[test]
fn test_begin_finger_revalidation_does_not_emit_a_lookup() -> Result<()> {
    let local = Did::from(0u32);
    // Revalidation only marks a range; scheduling emits the lookup separately.
    let seed = Did::from(8u32);
    let dht = PeerRing::new_with_storage(local, 3, Box::new(MemStorage::new()));
    let _ = dht.join(seed)?;

    assert!(matches!(
        dht.begin_finger_revalidation()?,
        PeerRingAction::None
    ));
    assert!(dht.finger_convergence_status()?.pending());
    Ok(())
}
