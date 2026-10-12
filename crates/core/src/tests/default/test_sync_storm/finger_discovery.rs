//! Finger discovery through the real transport: revalidating the local successor interval
//! stays local, and a discovery's whole transport cascade is measured.

use std::time::Duration;

use super::build_finger_nodes;
use super::close_nodes;
use super::connection_endpoints;
use super::drain_bootstrap;
use super::drain_untraced;
use super::establish_topology;
use super::install_chord_view;
use super::sorted_indices;
use super::ScenarioTopology;
use super::TEST_EPOCH_MS;
use crate::simulation::ProtectionProfile;
use crate::simulation::SimulationRuntimeGuard;
use crate::swarm::transport::outbound_submit_count_for_test;
use crate::swarm::transport::reset_outbound_submit_count_for_test;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

/// Five active dummy transports witness that revalidating the local successor
/// interval stays local instead of traversing the ring and reporting back.
#[tokio::test(start_paused = true)]
async fn test_five_node_local_successor_range_emits_no_finger_submission() {
    let runtime = SimulationRuntimeGuard::enter(767, TEST_EPOCH_MS, ProtectionProfile::ALL_ENABLED)
        .expect("local-range simulation runtime must install");
    let nodes = build_finger_nodes(&[3, 1, 10, 17, 29]);
    establish_topology(&runtime, &nodes, ScenarioTopology::Ring).await;
    install_chord_view(&nodes, ScenarioTopology::Ring);
    let observer = sorted_indices(&nodes).first().copied().unwrap_or(0);
    // Cancel the prepared local-successor request so the next convergence turn
    // exercises retry scheduling while the range remains locally provable.
    let request = nodes[observer]
        .dht()
        .lock_finger()
        .expect("observer finger table must be readable")
        .prepare_request_for_test(0)
        .expect("local successor slot must be a valid test request");
    nodes[observer]
        .dht()
        .cancel_finger_lookup(request)
        .expect("test request cancellation must succeed");
    runtime
        .advance(Duration::from_millis(2_000))
        .await
        .expect("finger retry floor must advance on the simulation clock");

    reset_outbound_submit_count_for_test();
    nodes[observer]
        .swarm
        .stabilizer()
        .converge_fingers_for_simulation()
        .await
        .expect("local successor range convergence must remain local");
    drain_untraced(&runtime, &nodes).await;
    assert_eq!(outbound_submit_count_for_test(), 0);

    let generations = connection_endpoints(&nodes)
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    close_nodes(&runtime, &nodes, &generations).await;
    drop(nodes);
    drop(runtime);
}

/// Production-path budget witness for one lookup that discovers a missing
/// finger peer. The measured submissions include lookup routing, its report,
/// and every admission follow-up caused while the new connection quiesces.
#[tokio::test(start_paused = true)]
async fn test_finger_discovery_measures_the_complete_transport_cascade() {
    /// Maximum control submissions allowed for this three-node convergence fixture.
    ///
    /// The bound includes routed lookup, proof report, connection admission,
    /// and every resulting topology follow-up before the fixture quiesces.
    const THREE_NODE_FIXTURE_MAX_CONTROL_SUBMISSIONS: usize = 20;

    let runtime = SimulationRuntimeGuard::enter(768, TEST_EPOCH_MS, ProtectionProfile::ALL_ENABLED)
        .expect("finger simulation runtime must install");
    let nodes = build_finger_nodes(&[3, 1, 10]);
    let (observer, seed, candidate) = finger_discovery_path(&nodes);

    // Build a two-hop knowledge path: observer can route to seed, seed knows
    // candidate, and observer has not already opened that connection.
    manually_establish_connection(&nodes[observer].swarm, &nodes[seed].swarm).await;
    drain_bootstrap(&runtime, &nodes).await;
    manually_establish_connection(&nodes[seed].swarm, &nodes[candidate].swarm).await;
    drain_bootstrap(&runtime, &nodes).await;
    assert!(nodes[seed]
        .dht()
        .successors()
        .list()
        .expect("seed successor view must be readable")
        .contains(&nodes[candidate].did()));
    assert!(nodes[observer]
        .swarm
        .transport
        .get_connection(nodes[candidate].did())
        .is_none());

    reset_outbound_submit_count_for_test();
    nodes[observer]
        .swarm
        .stabilizer()
        .converge_fingers_for_simulation()
        .await
        .expect("first routed production finger range must start");
    drain_untraced(&runtime, &nodes).await;
    // This is the complete production cascade, not just the lookup message and
    // its report.
    let submissions = outbound_submit_count_for_test();

    assert!(nodes[observer]
        .swarm
        .transport
        .get_connection(nodes[candidate].did())
        .is_some(),
        "candidate was not connected: observer_fingers={:?} seed_successors={:?} submissions={submissions}",
        nodes[observer]
            .dht()
            .lock_finger()
            .expect("observer finger table must be readable")
            .list(),
        nodes[seed]
            .dht()
            .successors()
            .list()
            .expect("seed successors must be readable"));
    assert!(
        submissions <= THREE_NODE_FIXTURE_MAX_CONTROL_SUBMISSIONS,
        "three-node finger discovery emitted {submissions} control submissions; fixture regression limit is {THREE_NODE_FIXTURE_MAX_CONTROL_SUBMISSIONS}"
    );

    let generations = connection_endpoints(&nodes)
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    close_nodes(&runtime, &nodes, &generations).await;
    drop(nodes);
    drop(runtime);
}

/// Select observer -> seed -> candidate such that the candidate sits in a
/// farther finger range than the seed. That makes the fixture prove routed
/// discovery instead of a direct successor/local-range update.
fn finger_discovery_path(nodes: &[Node]) -> (usize, usize, usize) {
    let sorted = sorted_indices(nodes);
    let observer = sorted.first().copied().unwrap_or(0);
    for (seed_position, seed) in sorted.iter().copied().enumerate().skip(1) {
        let seed_bits = crate::dht::topology::dist(nodes[observer].did(), nodes[seed].did()).bits();
        for candidate in sorted.iter().copied().skip(seed_position.saturating_add(1)) {
            let candidate_bits =
                crate::dht::topology::dist(nodes[observer].did(), nodes[candidate].did()).bits();
            if candidate_bits > seed_bits {
                return (observer, seed, candidate);
            }
        }
    }
    panic!("deterministic node fixture must contain two distinct finger ranges");
}
