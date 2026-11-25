use std::str::FromStr;

use num_bigint::BigUint;

use super::*;

/// The batch an admission emits: the successor-list query when `peer` newly
/// becomes referenced in topology (successor list or finger table), the connect
/// lookup, and, when `peer` became the successor head, the hand-off request the
/// topology head law adds.
fn admit_actions(
    peer: Did,
    local: Did,
    newly_referenced: bool,
    head_changed: bool,
) -> PeerRingAction {
    let mut actions = Vec::new();
    if newly_referenced {
        actions.push(PeerRingAction::RemoteAction(
            peer,
            RemoteAction::QueryForSuccessorList,
        ));
    }
    actions.push(PeerRingAction::RemoteAction(
        peer,
        RemoteAction::FindSuccessorForConnect(local),
    ));
    if head_changed {
        actions.push(PeerRingAction::StorageRepairDue);
    }
    PeerRingAction::MultiActions(actions)
}

/// Verifies that joins update finger hints in clockwise order across both the
/// ordinary interval and the identifier-space wraparound boundary.
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

    assert_eq!(
        node_a.admit_connected(a, None)?,
        PeerRingAction::MultiActions(Vec::new())
    );
    assert!(node_a.successors().is_empty()?);
    assert!(node_a.lock_finger()?.is_empty());

    assert_eq!(
        node_a.admit_connected(b, None)?,
        admit_actions(b, a, true, true)
    );
    assert!(BigUint::from(b) > BigUint::from(2u16).pow(156));
    assert!(BigUint::from(b) < BigUint::from(2u16).pow(157));

    // `b` covers slots below its highest set bit; larger slots remain unknown.
    let mut expected = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(None, 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![b]);

    for _ in 0..2 {
        node_a.admit_connected(b, None)?;
        assert_eq!(node_a.lock_finger()?.list(), &expected);
        assert_eq!(node_a.successors().list()?, vec![b]);
    }

    assert_eq!(
        node_a.admit_connected(c, None)?,
        admit_actions(c, a, true, false)
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
    assert_eq!(
        node_a.admit_connected(c, None)?,
        admit_actions(c, a, true, true)
    );
    let expected = std::iter::repeat_n(Some(c), 160).collect::<Vec<_>>();
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![c]);

    assert_eq!(
        node_a.admit_connected(b, None)?,
        admit_actions(b, a, true, true)
    );
    let mut expected = std::iter::repeat_n(Some(b), 157).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(Some(c), 3));
    assert_eq!(node_a.lock_finger()?.list(), &expected);
    assert_eq!(node_a.successors().list()?, vec![b, c]);

    let node_d = PeerRing::new_with_storage(d, 1, Box::new(MemStorage::new()));
    assert_eq!(
        node_d.admit_connected(a, None)?,
        admit_actions(a, d, true, true)
    );
    assert!(d + Did::from(BigUint::from(2u16).pow(151)) < a);
    assert!(d + Did::from(BigUint::from(2u16).pow(152)) > a);

    let mut expected = std::iter::repeat_n(Some(a), 152).collect::<Vec<_>>();
    expected.extend(std::iter::repeat_n(None, 8));
    assert_eq!(node_d.lock_finger()?.list(), &expected);
    assert_eq!(node_d.successors().list()?, vec![a]);

    assert_eq!(
        node_d.admit_connected(b, None)?,
        admit_actions(b, d, true, false)
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

/// The public `fix_fingers()` contract: one call both begins revalidation and
/// emits the first due lookup, rather than only marking the table stale for a
/// later maintenance tick.
///
/// Since 0.26 the two halves are separate transitions
/// (`begin_finger_revalidation` then `advance_finger_convergence`); this test
/// pins their composition behind the legacy entry point. The fixture has one
/// remote seed, so the lookup's next hop is unambiguous: seeing
/// `FindSuccessorForFix` routed to the seed witnesses the whole contract.
#[test]
fn test_public_fix_fingers_advances_one_range() -> Result<()> {
    let local = Did::from(0u32);
    let seed = Did::from(8u32);
    let dht = PeerRing::new_with_storage(local, 3, Box::new(MemStorage::new()));
    let _ = dht.admit_connected(seed, None)?;

    assert!(matches!(
        dht.fix_fingers()?,
        PeerRingAction::RemoteAction(
            next,
            RemoteAction::FindSuccessorForFix { .. }
        ) if next == seed
    ));
    Ok(())
}

/// Beginning revalidation only marks ranges stale; it emits no lookup, because
/// network work is paced separately by `advance_finger_convergence`.
///
/// The test expects no remote action from the begin step, and then that the
/// marked work is visible as pending to the scheduler.
#[test]
fn test_begin_finger_revalidation_does_not_emit_a_lookup() -> Result<()> {
    let local = Did::from(0u32);
    let seed = Did::from(8u32);
    let dht = PeerRing::new_with_storage(local, 3, Box::new(MemStorage::new()));
    let _ = dht.admit_connected(seed, None)?;

    assert!(matches!(
        dht.begin_finger_revalidation()?,
        PeerRingAction::None
    ));
    assert!(dht.finger_convergence_status()?.may_advance());
    Ok(())
}

#[test]
fn test_apply_fixed_finger_prunes_newly_referenced_peer_from_bridged() -> Result<()> {
    let local = Did::from(0u32);
    let seed = Did::from(8u32);
    let distant = Did::from_str("0x8000000000000000000000000000000000000000").unwrap();
    let dht = PeerRing::new_with_storage(local, 1, Box::new(MemStorage::new()));
    let _ = dht.admit_connected(seed, None)?;
    dht.record_bridged_peers(seed, vec![distant])?;
    assert_eq!(
        dht.topology_state()?.bridged.get(&distant).copied(),
        Some(seed)
    );

    let request = match dht.fix_fingers()? {
        PeerRingAction::RemoteAction(_, RemoteAction::FindSuccessorForFix { request, .. }) => {
            request
        }
        other => panic!("expected FindSuccessorForFix, got {other:?}"),
    };
    let outcome = dht.apply_fixed_finger(request, distant)?;
    assert_eq!(
        outcome,
        crate::dht::finger::FingerApplyOutcome::Applied,
        "expected Applied outcome, got {outcome:?}"
    );
    let state = dht.topology_state()?;
    assert!(state.references(distant));
    assert!(
        !state.bridged.contains_key(&distant),
        "apply_fixed_finger must prune newly referenced finger from bridged"
    );
    Ok(())
}

#[test]
fn test_apply_fixed_finger_preserves_bridged_routes_via_confirmed_peer_and_on_stale_proof(
) -> Result<()> {
    let local = Did::from(0u32);
    let near = Did::from(8u32);
    let far = Did::from_str("0x9000000000000000000000000000000000000000").unwrap();
    let other_bridged = Did::from_str("0xa000000000000000000000000000000000000000").unwrap();
    let distant_bridged = Did::from_str("0xb000000000000000000000000000000000000000").unwrap();
    let dht = PeerRing::new_with_storage(local, 1, Box::new(MemStorage::new()));
    let _ = dht.admit_connected(near, None)?;
    let _ = dht.admit_connected(far, None)?;
    dht.record_bridged_peers(far, vec![other_bridged, distant_bridged])?;
    let initial = dht.topology_state()?;
    assert_eq!(initial.bridged.get(&distant_bridged).copied(), Some(far));
    assert_eq!(initial.bridged.get(&other_bridged).copied(), Some(far));

    let request = match dht.fix_fingers()? {
        PeerRingAction::RemoteAction(_, RemoteAction::FindSuccessorForFix { request, .. }) => {
            request
        }
        other => panic!("expected FindSuccessorForFix, got {other:?}"),
    };

    // A stale/mismatched proof that does not install `distant_bridged` must not prune its bridged
    // hint or unrelated bridged entries via `far`.
    let stale_request =
        crate::dht::finger::FingerFixRequest::new(request.slot_index(), uuid::Uuid::new_v4())
            .expect("slot fits u16");
    let stale_outcome = dht.apply_fixed_finger(stale_request, distant_bridged)?;
    assert_ne!(
        stale_outcome,
        crate::dht::finger::FingerApplyOutcome::Applied,
        "stale proof must not be applied"
    );
    let after_stale = dht.topology_state()?;
    assert_eq!(
        after_stale.bridged.get(&distant_bridged).copied(),
        Some(far)
    );
    assert_eq!(after_stale.bridged.get(&other_bridged).copied(), Some(far));

    // Confirming an already-referenced peer (`far`) for an unverified finger slot must preserve
    // all active bridged hints relayed through `via == far`.
    let outcome = dht.apply_fixed_finger(request, far)?;
    assert_eq!(
        outcome,
        crate::dht::finger::FingerApplyOutcome::Applied,
        "expected Applied outcome for confirmed peer finger, got {outcome:?}"
    );
    let after_far = dht.topology_state()?;
    assert_eq!(after_far.bridged.get(&distant_bridged).copied(), Some(far));
    assert_eq!(after_far.bridged.get(&other_bridged).copied(), Some(far));
    Ok(())
}
