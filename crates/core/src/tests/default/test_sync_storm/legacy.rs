use super::pressure::exercise_bounded_control_burst;
use super::pressure::exercise_per_entry_yield;
use super::pressure::start_controlled_deliveries;
use super::pressure::wait_for_control_barrier_verdict;
use super::*;

pub(super) async fn legacy_feedback_loop_state() -> SimState {
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
                "replay_command": "cargo test -p rings-core --features dummy --no-default-features test_legacy_all_disabled_reproduces_complete_feedback_loop -- --nocapture",
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
    expire_healthy_peer(&runtime, &nodes, observer, peer, &mut driver).await;
    driver.stop_storm();
    persist_runtime_artifact("legacy-feedback-complete", &runtime)
        .expect("legacy feedback artifact must be writable");
    assert_eq!(
        runtime
            .protection_observations()
            .expect("legacy observations must remain available")
            .violations(),
        &ProtectionProfile::LEGACY_ALL_DISABLED.disabled_layers()
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
    dummy_controlled::set_max_message_size(crate::consts::TRANSPORT_MTU);
    for index in 0..250 {
        let entry = entry_owned_by(&nodes[peer], &format!("legacy-loop-{index}"));
        entries.push(entry);
    }
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(peer_did),
        data: entries.clone(),
    };
    assert!(matches!(
        nodes[observer]
            .swarm
            .transport
            .send_storage_sync_tracked(msg)
            .await
            .expect("legacy sync must enter the real scheduler"),
        TrackedStorageSyncOutcome::Delivered(_)
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

async fn expire_healthy_peer(
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
        "the false-disconnect clock must originate at the real stabilizer probe"
    );
    let probe = observe_stabilizer_probe(runtime, driver);
    let backlog = runtime
        .pending_deliveries()
        .expect("legacy reassembly backlog must classify")
        .into_iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Reassembly)
        .take(28)
        .collect::<Vec<_>>();
    assert_eq!(
        backlog.len(),
        28,
        "legacy storm must fill the real inbound reassembly lane"
    );
    runtime
        .enable_reassembly_service()
        .expect("legacy reassembly service must enable");
    let mut backlog_deliveries = start_controlled_deliveries(runtime, backlog.clone());
    assert!(matches!(
        futures::poll!(backlog_deliveries.next()),
        std::task::Poll::Pending
    ));
    settle_one_poll().await;
    for delivery in &backlog {
        driver.observe_dispatch(delivery);
    }
    let mut probe_delivery = runtime.deliver(&probe).boxed_local();
    assert!(!wait_for_control_barrier_verdict(runtime, probe_delivery.as_mut(), true).await);
    driver.observe_dispatch(&probe);
    let barrier_event = driver.observe_barrier(&probe, true);
    persist_inflight_trace_artifact("legacy-probe-starved", runtime, &driver.state)
        .expect("legacy probe trace artifact must be writable");
    let liveness_timeout_ms = u64::try_from(PEER_LIVENESS_TIMEOUT_MS)
        .expect("production liveness timeout must be positive")
        .saturating_add(1);
    advance_liveness_deadline(runtime, driver, liveness_timeout_ms).await;
    record_probe_deadline_miss(runtime, &probe, probe_delivery.as_mut()).await;
    observe_false_disconnect(
        nodes,
        observer,
        peer,
        barrier_event,
        probe.transaction_id,
        driver,
    )
    .await;
    drop(probe_delivery);
    drop(backlog_deliveries);
    runtime
        .disable_reassembly_service()
        .expect("legacy reassembly service must disable");
    settle_one_poll().await;
    persist_inflight_trace_artifact("legacy-false-disconnect", runtime, &driver.state)
        .expect("legacy disconnect trace artifact must be writable");
    observe_repair_feedback(runtime, nodes, observer, driver).await;
}

fn observe_stabilizer_probe(
    runtime: &SimulationRuntimeGuard,
    driver: &mut TraceDriver,
) -> ScheduledDelivery {
    let new_deliveries = runtime
        .new_pending_deliveries()
        .expect("stabilizer probe queue must classify");
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

async fn record_probe_deadline_miss<F>(
    runtime: &SimulationRuntimeGuard,
    probe: &ScheduledDelivery,
    mut probe_delivery: std::pin::Pin<&mut F>,
) where
    F: std::future::Future<Output = Result<bool, crate::simulation::SimulationRuntimeError>>
        + ?Sized,
{
    let observed = u64::try_from(runtime.elapsed_ms().expect("elapsed time must fit"))
        .expect("legacy elapsed time must fit u64");
    let deadline = probe
        .deadline_virtual_ms
        .expect("the exact stabilizer probe must carry its production deadline");
    assert!(observed > deadline);
    assert!(matches!(
        futures::poll!(probe_delivery.as_mut()),
        std::task::Poll::Pending
    ));
    crate::simulation::record_barrier_control_deadline_miss(observed, deadline);
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

async fn observe_false_disconnect(
    nodes: &[Node],
    observer: usize,
    peer: usize,
    probe_parent: SimEventId,
    probe_transaction: Option<uuid::Uuid>,
    driver: &mut TraceDriver,
) {
    let peer_did = nodes[peer].did();
    nodes[observer]
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await
        .expect("expired liveness evidence must be processed");
    assert!(nodes[observer]
        .swarm
        .transport
        .get_connection(peer_did)
        .is_none());
    assert!(nodes[observer].swarm.transport.storage_repair_requested());
    let local = nodes[observer].did().to_string();
    let remote = nodes[peer].did().to_string();
    let generation = driver
        .endpoints
        .iter()
        .find_map(|(generation, endpoints)| {
            (endpoints == &(local.clone(), remote.clone())).then(|| generation.clone())
        })
        .expect("false liveness verdict must name the removed production generation");
    let verdict = driver.observe_liveness_verdict(
        SimNodeId(observer as u16),
        SimNodeId(peer as u16),
        generation,
        probe_transaction,
        true,
        Some(probe_parent),
    );
    let (state, disconnect) = driver
        .take_state()
        .transition(SimAction::Disconnect {
            node: SimNodeId(observer as u16),
            peer: SimNodeId(peer as u16),
            causal_parent: Some(verdict),
        })
        .expect("false disconnect observation must be valid");
    driver.state = state;
    driver.record_node_event(SimNodeId(observer as u16), disconnect.event_id);
}

async fn observe_repair_feedback(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    observer: usize,
    driver: &mut TraceDriver,
) {
    let remaining_peer = nodes
        .iter()
        .find(|node| {
            node.did() != nodes[observer].did()
                && nodes[observer]
                    .swarm
                    .transport
                    .get_connection(node.did())
                    .is_some()
        })
        .expect("legacy repair requires one still-admitted peer")
        .did();
    nodes[observer]
        .dht()
        .join(remaining_peer)
        .expect("remaining repair peer must be routable");
    nodes[observer]
        .dht()
        .notify(remaining_peer)
        .expect("remaining repair predecessor must be visible");
    let repair_entry = entry_routed_remotely_by(&nodes[observer], "legacy-repair-feedback");
    nodes[observer]
        .dht()
        .storage
        .put(&repair_entry.key.to_string(), &repair_entry.entry)
        .await
        .expect("legacy repair source entry must be stored");
    let submissions_before = outbound_submit_count_for_test();
    let pending_before = runtime
        .pending_deliveries()
        .expect("pre-repair queue must classify")
        .into_iter()
        .map(|delivery| delivery.sequence)
        .collect::<BTreeSet<_>>();
    let stabilizer = nodes[observer].swarm.stabilizer();
    let outcome = stabilizer
        .run_requested_storage_repair()
        .await
        .expect("claimed repair request must execute a bounded pass");
    assert!(
        outcome.is_complete(),
        "repair feedback pass must be delivered"
    );
    let repair_entries = runtime
        .repair_entries_observed()
        .expect("repair observation must remain available");
    let submissions_after = outbound_submit_count_for_test();
    let new_repair_frames = runtime
        .pending_deliveries()
        .expect("post-repair queue must classify")
        .into_iter()
        .filter(|delivery| !pending_before.contains(&delivery.sequence))
        .collect::<Vec<_>>();
    assert!(
        repair_entries > 0,
        "production repair must emit real entries: outcome={outcome:?} submissions={submissions_before}->{submissions_after} new_frames={:?}",
        new_repair_frames
            .iter()
            .map(|delivery| delivery.class)
            .collect::<Vec<_>>()
    );
    assert!(
        submissions_after > submissions_before,
        "production repair must cross a new outbound submission boundary"
    );
    assert!(
        new_repair_frames.iter().any(|delivery| matches!(
            delivery.class,
            ScheduledDeliveryClass::Storage | ScheduledDeliveryClass::Reassembly
        )),
        "completed production repair must append a distinct data-plane frame"
    );
    let parent = driver.last_event;
    let (state, output) = driver
        .take_state()
        .transition(SimAction::ScheduleRepair {
            node: SimNodeId(observer as u16),
            entries: repair_entries,
            causal_parent: Some(parent),
        })
        .expect("observed repair feedback must be valid");
    driver.state = state;
    driver.record_node_event(SimNodeId(observer as u16), output.event_id);
}

fn entry_routed_remotely_by(node: &Node, label: &str) -> PlacedEntry {
    for nonce in 0_u64..100_000 {
        let topic = format!("sync-storm-{label}-{nonce}");
        let key = Entry::gen_did(&topic).expect("repair fixture DID must derive");
        if matches!(
            node.dht()
                .find_storage_owner(key)
                .expect("repair route must resolve"),
            PeerRingAction::RemoteAction(_, _)
        ) {
            let data = vec![0x6d; ENTRY_PAYLOAD_BYTES]
                .encode()
                .expect("repair fixture payload must encode");
            return PlacedEntry::new(key, Entry::new(key, vec![data], EntryKind::Data));
        }
    }
    panic!("failed to derive a remote repair entry from {}", node.did());
}
