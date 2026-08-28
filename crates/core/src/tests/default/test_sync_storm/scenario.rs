//! Scenario orchestration kept separate from protocol and pressure helpers.

use super::*;

async fn exercise_pressure_suite(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
    driver: &mut TraceDriver,
) -> (&'static str, serde_json::Value) {
    let (pressure_node, pressure_peer) = physical_edges(nodes, kind)[0];
    let overload = nodes[pressure_node]
        .swarm
        .transport
        .exercise_class_reservation_pressure_for_simulation(nodes[pressure_peer].did())
        .expect("live class-reservation pressure must be observable");
    let overload_witness = typed_overload_witness(&overload);
    persist_runtime_artifact("class-reservation-pressure", runtime)
        .expect("class-reservation pressure artifact must be writable");
    exercise_bounded_control_burst(runtime, nodes, kind).await;
    dummy_controlled::set_max_message_size(CHUNK_MESSAGE_SIZE);
    exercise_barrier_control_exemption(runtime, nodes, kind, driver).await;
    exercise_per_entry_yield(runtime, nodes, kind).await;
    driver.observe_production_handlers(runtime);
    let pressure_virtual_ms = u64::try_from(
        runtime
            .elapsed_ms()
            .expect("pressure time must remain visible"),
    )
    .expect("pressure time must fit the model");
    driver.advance_virtual_to(pressure_virtual_ms);
    let snapshot =
        runtime_replay_snapshot(runtime).expect("pressure replay snapshot must remain observable");
    (overload_witness, snapshot)
}

async fn run_storm_and_recovery(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
    strategy: DeliveryStrategy,
    expected_entries: usize,
    expected_endpoints: &BTreeMap<String, (String, String)>,
    driver: &mut TraceDriver,
) -> u128 {
    let idle_ms = u64::try_from(PEER_LIVENESS_IDLE_MS)
        .expect("liveness idle interval must be positive")
        .saturating_add(1);
    runtime
        .advance(Duration::from_millis(idle_ms))
        .await
        .expect("liveness idle interval must advance deterministically");
    driver.advance_virtual(idle_ms);
    assert_eq!(submit_workload(nodes, kind).await, expected_entries);
    let storm_backlog = runtime
        .new_pending_deliveries()
        .expect("storm backlog must remain observable before maintenance");
    assert!(
        !storm_backlog.is_empty(),
        "maintenance must run while the submitted storm is still queued"
    );
    assert!(storm_backlog
        .iter()
        .any(|delivery| delivery.class == ScheduledDeliveryClass::Control));
    assert!(storm_backlog.iter().any(|delivery| matches!(
        delivery.class,
        ScheduledDeliveryClass::Storage | ScheduledDeliveryClass::Reassembly
    )));
    driver.observe_pending(runtime, &storm_backlog);
    run_empty_repair_maintenance(nodes, driver).await;
    assert!(
        !runtime
            .pending_deliveries()
            .expect("storm backlog must remain observable after maintenance")
            .is_empty(),
        "maintenance must not consume the queued storm"
    );
    let probes = begin_liveness_under_storm(runtime, nodes, driver).await;
    let started_ms = runtime
        .elapsed_ms()
        .expect("recovery start must remain observable");
    drain_traced(runtime, nodes, strategy, driver).await;
    conclude_healthy_liveness(nodes, expected_endpoints, &probes, driver).await;
    driver.stop_storm();
    runtime
        .elapsed_ms()
        .expect("simulation elapsed time must remain observable")
        .saturating_sub(started_ms)
}

fn terminal_observations(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
    recovery_elapsed_ms: u128,
    expected_entries: usize,
    active_peer_queues: usize,
) -> (
    BTreeSet<ProtectionLayer>,
    ProtectionObservations,
    ProductionCapacityObservations,
) {
    let protection_observations = runtime
        .protection_observations()
        .expect("production protection observations must remain available");
    let protection_violations = protection_observations.violations().clone();
    let capacity_observations = runtime
        .capacity_observations()
        .expect("production capacity observations must remain available");
    assert_eq!(dummy_controlled::pending(), 0);
    assert!(!network_busy(nodes));
    assert_healthy_connections(nodes, kind);
    assert!(nodes
        .iter()
        .all(|node| !node.swarm.transport.storage_repair_requested()));
    assert!(
        recovery_elapsed_ms <= recovery_bound_ms(expected_entries, active_peer_queues),
        "recovery exceeded the capacity/service bound: elapsed={recovery_elapsed_ms} bound={}",
        recovery_bound_ms(expected_entries, active_peer_queues),
    );
    (
        protection_violations,
        protection_observations,
        capacity_observations,
    )
}

pub(super) async fn run_scenario(
    count: usize,
    kind: ScenarioTopology,
    seed: u64,
    profile: ProtectionProfile,
    strategy: DeliveryStrategy,
) -> ScenarioOutcome {
    assert!(count >= 3);
    let runtime = SimulationRuntimeGuard::enter(seed, TEST_EPOCH_MS, profile)
        .expect("simulation runtime must install");
    runtime
        .set_artifact_identity(
            format!(
                "{}-n{count}-seed{seed}-{}-{}",
                kind.name(),
                profile.name(),
                strategy.name()
            ),
            serde_json::json!({
                "topology": kind.name(),
                "count": count,
                "seed": seed,
                "profile": profile.name(),
                "strategy": strategy.name(),
                "replay_command": scenario_replay_command(count, kind, seed, profile, strategy),
            }),
        )
        .expect("scenario artifact identity must install");
    let failure_guard = ScenarioFailureGuard::new(&runtime);
    let nodes = build_nodes(count).await;
    establish_topology(&runtime, &nodes, kind).await;
    install_chord_view(&nodes, kind);
    let expected_entries = workload_edges(&nodes, kind).len();
    let endpoints = connection_endpoints(&nodes);
    let active_peer_queues = endpoints.len();
    let generations = endpoints.keys().cloned().collect::<Vec<_>>();
    let initial_virtual_ms = u64::try_from(
        runtime
            .elapsed_ms()
            .expect("initial simulation time must remain visible"),
    )
    .expect("initial simulation time must fit the model");
    let mut driver = TraceDriver::new(
        expected_entries,
        endpoints.clone(),
        node_ids(&nodes),
        initial_virtual_ms,
        failure_guard.diagnostics(),
    );
    let (overload_witness, pressure_snapshot) =
        exercise_pressure_suite(&runtime, &nodes, kind, &mut driver).await;
    let recovery_elapsed_ms = run_storm_and_recovery(
        &runtime,
        &nodes,
        kind,
        strategy,
        expected_entries,
        &endpoints,
        &mut driver,
    )
    .await;
    let persisted_by_node = count_persisted(&nodes).await;
    let persisted_entries = persisted_by_node.iter().sum();
    driver.observe_production_handlers(&runtime);
    let (protection_violations, protection_observations, capacity_observations) =
        terminal_observations(
            &runtime,
            &nodes,
            kind,
            recovery_elapsed_ms,
            expected_entries,
            active_peer_queues,
        );
    dummy_controlled::set_max_message_size(0);
    close_nodes(&runtime, &nodes, &generations).await;
    driver.observe_lifecycle(SimConnectionState::Closed);
    let outcome = ScenarioOutcome {
        state: driver.state,
        persisted_entries,
        expected_entries,
        count,
        topology: kind,
        seed,
        profile,
        strategy,
        protection_violations,
        protection_observations,
        capacity_observations,
        pressure_snapshot,
        recovery_elapsed_ms,
        overload_witness,
    };
    persist_trace_artifact(&outcome).expect("configured trace artifact must be writable");
    drop(nodes);
    failure_guard.disarm();
    drop(failure_guard);
    drop(runtime);
    outcome
}
