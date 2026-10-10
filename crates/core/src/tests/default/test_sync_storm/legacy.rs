use super::pressure::await_one_handover;
use super::pressure::drain_started_reassembly;
use super::pressure::exercise_bounded_control_burst;
use super::pressure::exercise_per_entry_yield;
use super::pressure::in_flight_reassembly;
use super::pressure::new_deliveries_with_controls;
use super::pressure::start_lane_handover;
use super::pressure::wait_for_control_barrier_verdict;
use super::*;
use crate::swarm::transport::StorageSyncSend;

/// The legacy storm, every protection layer disabled, against per-lane credit dispatch.
///
/// Before #924 this reproduced the storm's feedback loop: a reassembly backlog behind the
/// barrier starved a liveness probe past its deadline, the peer was falsely disconnected, and
/// the repair that followed refed the storm. Under per-lane credit dispatch the loop breaks at
/// its first step: the barrier still blocks the probe, but only behind the one reassembly frame
/// being handed over, so the probe is answered in time and the peer stays connected. The
/// per-entry-yield proposition, which credit does not subsume, is still violated.
pub(super) async fn legacy_storm_state() -> SimState {
    let runtime =
        SimulationRuntimeGuard::enter(900, TEST_EPOCH_MS, ProtectionProfile::LEGACY_ALL_DISABLED)
            .expect("legacy simulation runtime must install");
    runtime
        .set_artifact_identity(
            "legacy-ring-n3-seed900-legacy-all-disabled-fifo".to_owned(),
            serde_json::json!({
                "topology": "ring",
                "count": 3,
                "seed": 900,
                "profile": "legacy-all-disabled",
                "strategy": "fifo",
                "replay_command": "cargo test -p rings-core --features dummy --no-default-features test_legacy_all_disabled_breaks_the_feedback_loop_at_the_barrier -- --nocapture",
            }),
        )
        .expect("legacy artifact identity must install");
    let failure_guard = ScenarioFailureGuard::new(&runtime);
    let nodes = build_repair_nodes(3);
    establish_topology(&runtime, &nodes, ScenarioTopology::Ring).await;
    install_chord_view(&nodes, ScenarioTopology::Ring);
    let (pressure_node, pressure_peer) = physical_edges(&nodes, ScenarioTopology::Ring)[0];
    let overload = nodes[pressure_node]
        .swarm
        .transport
        .exercise_class_reservation_pressure_for_simulation(nodes[pressure_peer].did())
        .expect("legacy admission pressure must be observable");
    let _overload_witness = typed_overload_witness(&overload);
    exercise_bounded_control_burst(&runtime, &nodes, ScenarioTopology::Ring).await;
    dummy_controlled::set_max_message_size(CHUNK_MESSAGE_SIZE);
    exercise_per_entry_yield(&runtime, &nodes, ScenarioTopology::Ring).await;

    let (observer, peer, mut driver) =
        queue_legacy_storm(&runtime, &nodes, failure_guard.diagnostics()).await;
    persist_runtime_artifact("legacy-storm-queued", &runtime)
        .expect("legacy queued-storm artifact must be writable");
    let generations = connection_endpoints(&nodes)
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    probe_healthy_peer_behind_barrier(&runtime, &nodes, observer, peer, &mut driver).await;
    driver.stop_storm();
    persist_runtime_artifact("legacy-feedback-broken", &runtime)
        .expect("legacy feedback artifact must be writable");
    // Every disabled layer is violated except the barrier exemption, which per-lane credit
    // dispatch subsumes (#927).
    let mut violated = ProtectionProfile::LEGACY_ALL_DISABLED.disabled_layers();
    violated.remove(&ProtectionLayer::BarrierControlExemption);
    assert_eq!(
        runtime
            .protection_observations()
            .expect("legacy observations must remain available")
            .violations(),
        &violated
    );

    dummy_controlled::set_max_message_size(0);
    close_nodes(&runtime, &nodes, &generations).await;
    driver.observe_lifecycle(SimConnectionState::Closed);
    persist_named_trace_artifact(
        "legacy-ring-n3-seed900-legacy-all-disabled-fifo-feedback-trace",
        &runtime,
        &driver.state,
    )
    .expect("legacy semantic trace artifact must be writable");
    drop(nodes);
    failure_guard.disarm();
    drop(failure_guard);
    drop(runtime);
    driver.state
}

async fn queue_legacy_storm(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    failure: FailureState,
) -> (usize, usize, TraceDriver) {
    let sorted = sorted_indices(nodes);
    let observer = sorted[0];
    let peer = sorted[1];
    let peer_did = nodes[peer].did();
    let mut entries = Vec::new();
    dummy_controlled::set_max_message_size(super::CHUNKED_MAX_MESSAGE_SIZE);
    for index in 0..250 {
        let entry = entry_owned_by(&nodes[peer], &format!("legacy-loop-{index}"));
        entries.push(entry);
    }
    // Sent detached: under flow control a transfer longer than the lane's credit window cannot
    // complete before the receiver consumes it, so the worker feeds it as credit returns.
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(peer_did),
        data: entries.clone(),
    };
    assert!(matches!(
        nodes[observer]
            .swarm
            .transport
            .send_storage_sync_or_defer(msg, StorageSyncSend::Admitted, "test")
            .await
            .expect("legacy sync must enter the real scheduler"),
        StorageSyncOutcome::Sent(_)
    ));
    let initial_virtual_ms = u64::try_from(
        runtime
            .elapsed_ms()
            .expect("legacy initial time must remain visible"),
    )
    .expect("legacy initial time must fit the model");
    let mut driver = TraceDriver::new(
        entries.len(),
        connection_endpoints(nodes),
        node_ids(nodes),
        initial_virtual_ms,
        failure,
    );
    let pending = runtime
        .pending_deliveries()
        .expect("legacy queue must classify");
    driver.observe_pending(runtime, &pending);
    assert!(pending
        .iter()
        .any(|delivery| delivery.class == ScheduledDeliveryClass::Reassembly));

    (observer, peer, driver)
}

/// Probe the healthy `peer` while its reassembly backlog holds the barrier: the probe is
/// blocked, answered within one hand-over, and the peer is not disconnected.
async fn probe_healthy_peer_behind_barrier(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    observer: usize,
    peer: usize,
    driver: &mut TraceDriver,
) {
    let peer_did = nodes[peer].did();
    let idle_ms = u64::try_from(PEER_LIVENESS_IDLE_MS)
        .expect("production liveness idle interval must be positive")
        .saturating_add(1);
    advance_liveness_deadline(runtime, driver, idle_ms).await;
    refresh_non_target_peer_liveness(runtime, nodes, observer, peer).await;
    nodes[observer]
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await
        .expect("production stabilizer must send its real liveness probe");
    let sent_at_ms = i64::try_from(
        TEST_EPOCH_MS.saturating_add(
            runtime
                .elapsed_ms()
                .expect("elapsed time must remain visible"),
        ),
    )
    .expect("simulated epoch must fit liveness representation");
    assert_eq!(
        nodes[observer]
            .swarm
            .transport
            .peer_liveness_unanswered_since_for_test(peer_did)
            .expect("active peer liveness state must remain readable"),
        Some(sent_at_ms),
        "the liveness clock must originate at the real stabilizer probe"
    );
    let probe = observe_stabilizer_probe(runtime, driver).await;
    deliver_probe_behind_barrier(runtime, driver, &probe).await;
    deliver_probe_answer(runtime, nodes, driver, &probe).await;
    // The probe was answered: no unanswered probe remains to expire into a false disconnect,
    // the observer still holds the peer, and no repair entry is emitted.
    assert_eq!(
        nodes[observer]
            .swarm
            .transport
            .peer_liveness_unanswered_since_for_test(peer_did)
            .expect("active peer liveness state must remain readable"),
        None,
        "the probe must be answered once the barrier released it"
    );
    nodes[observer]
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await
        .expect("liveness evidence must be processed");
    assert!(nodes[observer]
        .swarm
        .transport
        .get_connection(peer_did)
        .is_some());
    assert_eq!(
        runtime
            .repair_entries_observed()
            .expect("repair observation must remain available"),
        0,
        "no repair follows an answered probe"
    );
    persist_inflight_trace_artifact("legacy-probe-answered", runtime, &driver.state)
        .expect("legacy probe trace artifact must be writable");
}

/// Deliver `probe` while the peer's reassembly backlog holds the barrier: it is blocked, and
/// delivered within one hand-over of the backlog, which then drains.
async fn deliver_probe_behind_barrier(
    runtime: &SimulationRuntimeGuard,
    driver: &mut TraceDriver,
    probe: &ScheduledDelivery,
) {
    let deadline = probe
        .deadline_virtual_ms
        .expect("the exact stabilizer probe must carry its production deadline");
    let backlog = in_flight_reassembly(runtime);
    runtime
        .enable_reassembly_service()
        .expect("legacy reassembly service must enable");
    let mut backlog_deliveries = start_lane_handover(runtime, backlog.clone()).await;
    settle_one_poll().await;
    for delivery in &backlog {
        driver.observe_dispatch(delivery);
    }
    let mut probe_delivery = runtime.deliver(probe).boxed_local();
    assert!(!wait_for_control_barrier_verdict(runtime, probe_delivery.as_mut(), true).await);
    driver.observe_dispatch(probe);
    driver.observe_barrier(probe, true);
    await_one_handover(runtime, probe_delivery.as_mut(), &backlog, deadline).await;
    driver.observe_delivery(runtime, probe);
    drop(probe_delivery);
    drain_started_reassembly(runtime, &mut backlog_deliveries).await;
    for delivery in &backlog {
        driver.observe_delivery(runtime, delivery);
    }
    runtime
        .disable_reassembly_service()
        .expect("legacy reassembly service must disable");
}

/// Deliver the peer's answer to `probe`, under the probe's transaction: the liveness evidence
/// the barrier would have withheld past the deadline before #924.
async fn deliver_probe_answer(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut TraceDriver,
    probe: &ScheduledDelivery,
) {
    let answer = runtime
        .pending_deliveries()
        .expect("probe answer must classify")
        .into_iter()
        .find(|delivery| {
            delivery.class == ScheduledDeliveryClass::Control
                && delivery.transaction_id == probe.transaction_id
        })
        .expect("the peer must answer the probe once the barrier released it");
    assert!(runtime
        .deliver(&answer)
        .await
        .expect("probe answer delivery must remain stable"));
    driver.observe_delivery(runtime, &answer);
    drain_bootstrap(runtime, nodes).await;
}

async fn observe_stabilizer_probe(
    runtime: &SimulationRuntimeGuard,
    driver: &mut TraceDriver,
) -> ScheduledDelivery {
    let new_deliveries = new_deliveries_with_controls(runtime, 1).await;
    driver.observe_pending(runtime, &new_deliveries);
    let probes = new_deliveries
        .iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Control)
        .collect::<Vec<_>>();
    assert_eq!(
        probes.len(),
        1,
        "refreshing the non-target peer must isolate one real stabilizer probe: {new_deliveries:?}"
    );
    let probe = (*probes[0]).clone();
    assert!(probe.transaction_id.is_some());
    assert!(probe.deadline_virtual_ms.is_some());
    probe
}

async fn refresh_non_target_peer_liveness(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    observer: usize,
    target: usize,
) {
    let other = (0..nodes.len())
        .find(|index| *index != observer && *index != target)
        .expect("legacy topology must contain a non-target peer");
    nodes[other]
        .swarm
        .send_direct_message(
            Message::custom(b"legacy-non-target-liveness")
                .expect("liveness refresh payload must encode"),
            nodes[observer].did(),
        )
        .await
        .expect("non-target liveness refresh must enter the scheduler");
    let refresh = runtime
        .pending_deliveries()
        .expect("liveness refresh queue must classify")
        .into_iter()
        .find(|delivery| delivery.class == ScheduledDeliveryClass::Application)
        .expect("non-target liveness refresh frame must be queued");
    assert!(runtime
        .deliver(&refresh)
        .await
        .expect("liveness refresh delivery must remain stable"));
}

async fn advance_liveness_deadline(
    runtime: &SimulationRuntimeGuard,
    driver: &mut TraceDriver,
    liveness_timeout_ms: u64,
) {
    runtime
        .advance(Duration::from_millis(liveness_timeout_ms))
        .await
        .expect("legacy virtual deadline must advance");
    driver.advance_virtual(liveness_timeout_ms);
}
