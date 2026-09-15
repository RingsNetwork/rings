//! Unit coverage for topology-report filtering and bounded candidate selection.

use super::confirmed_topology;
use super::topology_has_confirmed_peer;
use crate::dht::topology::bounded_connection_candidates;
use crate::dht::topology::StabilizationConnectionPlan;
use crate::dht::Did;
use crate::dht::TopoInfo;
use crate::ecc::SecretKey;

/// Proves that topology filtering retains only peers accepted by the current
/// transport-evidence predicate.
///
/// One successor is marked active while an additional successor and the
/// predecessor remain pending. The filtered report must contain only the active
/// peer and still report usable topology.
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

/// Proves the stabilization candidate budget, order, and de-duplication law
/// as the handler applies it: predecessor first, then successors, local and
/// duplicate DIDs removed, at most successor capacity plus one.
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
            Did::from(5u32),
        ],
    };

    let candidates = bounded_connection_candidates(
        local,
        StabilizationConnectionPlan::candidate_capacity(3),
        report
            .predecessor
            .into_iter()
            .chain(report.successors.iter().copied()),
    );

    assert_eq!(candidates, vec![
        predecessor,
        Did::from(2u32),
        Did::from(3u32),
        Did::from(4u32)
    ]);
}

/// Proves that successor-sync candidate selection ignores predecessor hints
/// and emits only bounded unique remote successors, in the order given.
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

    let candidates = bounded_connection_candidates(local, 3, report.successors.iter().copied());

    assert_eq!(candidates, vec![
        Did::from(1u32),
        Did::from(2u32),
        Did::from(3u32)
    ]);
}
