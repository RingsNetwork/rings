use super::confirmed_topology;
use super::topology_has_confirmed_peer;
use crate::dht::Did;
use crate::dht::TopoInfo;
use crate::ecc::SecretKey;

#[test]
fn test_topology_report_keeps_only_confirmed_peers() {
    let active = SecretKey::random().address().into();
    let pending_successor = SecretKey::random().address().into();
    let pending_predecessor = SecretKey::random().address().into();
    let confirmed = confirmed_topology(
        &TopoInfo {
            successors: vec![active, pending_successor],
            predecessor: Some(pending_predecessor),
        },
        |peer| peer == active,
    );

    assert_eq!(confirmed.successors, vec![active]);
    assert_eq!(confirmed.predecessor, None);
    assert!(topology_has_confirmed_peer(&confirmed));
}

#[test]
fn test_stabilization_candidate_effects_are_deduplicated_and_capacity_bounded() {
    let local = Did::from(0u32);
    let predecessor = Did::from(1u32);
    let report = TopoInfo {
        predecessor: Some(predecessor),
        successors: vec![
            predecessor,
            local,
            Did::from(2u32),
            Did::from(3u32),
            Did::from(4u32),
        ],
    };

    let candidates = report.connection_candidates(local, 3);

    assert_eq!(candidates, vec![
        predecessor,
        Did::from(2u32),
        Did::from(3u32)
    ]);
    assert!(candidates.len() <= 4);
}

#[test]
fn test_sync_candidate_effects_are_deduplicated_and_capacity_bounded() {
    let local = Did::from(0u32);
    let report = TopoInfo {
        predecessor: Some(Did::from(9u32)),
        successors: vec![
            local,
            Did::from(1u32),
            Did::from(1u32),
            Did::from(2u32),
            Did::from(3u32),
            Did::from(4u32),
        ],
    };

    let candidates = report.successor_connection_candidates(local, 3);

    assert_eq!(candidates, vec![
        Did::from(1u32),
        Did::from(2u32),
        Did::from(3u32)
    ]);
}
