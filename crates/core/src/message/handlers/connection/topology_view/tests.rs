//! Unit coverage for topology-report filtering and bounded candidate selection.

use super::confirmed_topology;
use super::topology_has_confirmed_peer;
use crate::dht::Did;
use crate::dht::TopoInfo;
use crate::ecc::SecretKey;

#[test]
fn test_topology_report_keeps_only_confirmed_peers() {
    // `active` is the only DID the production predicate would accept as
    // currently routable; the pending peers model unadmitted topology evidence.
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
    // The predecessor is considered first, then successor hints are de-duped
    // and truncated to successor capacity.
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
    // The production bound can hold predecessor plus successor-capacity peers;
    // this fixture only needs three after de-duplication.
    assert!(candidates.len() <= 4);
}

#[test]
fn test_sync_candidate_effects_are_deduplicated_and_capacity_bounded() {
    let local = Did::from(0u32);
    // Successor sync ignores predecessor hints and keeps only remote successors
    // that fit in the local successor-list capacity.
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
