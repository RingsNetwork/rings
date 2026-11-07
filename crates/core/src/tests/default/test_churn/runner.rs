//! Production churn scenario runner driving real `Swarm`, `PeerRing`, and `Stabilizer` phases.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

use bytes::Bytes;
use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;

use super::driver::ChurnTraceDriver;
use super::inbox::deliver_rejoined_inbox_messages;
use super::inbox::inject_offline_inbox_messages;
use super::inbox::sample_ring_liveness_and_lookups;
use super::inbox::LivenessSampleCounters;
use super::ChurnMetrics;
use super::ChurnScenarioConfig;
use super::ChurnScenarioOutcome;
use super::CHURN_MODEL_LIMITS;
use super::TEST_EPOCH_MS;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::topology;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::StorageRepairOutcome;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::tests::deterministic_key;
use crate::message::SyncEntriesWithSuccessor;
use crate::simulation::model::ScheduledChurnEvent;
use crate::simulation::model::SimAction;
use crate::simulation::model::SimMaintenanceOutcome;
use crate::simulation::model::SimNodeId;
use crate::simulation::model::WeibullChurnConfig;
use crate::simulation::model::WeibullSessionGenerator;
use crate::simulation::DeliveryStrategy;
use crate::simulation::ProtectionProfile;
use crate::simulation::ScheduledDeliveryClass;
use crate::simulation::SimulationRuntimeGuard;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::transport::StorageSyncOutcome;
use crate::tests::default::prepare_node;
use crate::tests::default::Node;
use crate::tests::live_entry;
use crate::tests::manually_establish_connection;

const MAX_DRAIN_STEPS: usize = 100_000;
const QUIESCENT_POLLS: usize = 6;
const MAX_IN_FLIGHT_POLLS: usize = 32;
const VIRTUAL_SERVICE_BYTES_PER_MS: usize = 512;
const MAX_FRAME_SERVICE_MS: u64 = 16;
const INITIAL_STORAGE_PAYLOAD_BYTES: usize = 512;

pub(super) async fn run_churn_scenario(config: ChurnScenarioConfig) -> ChurnScenarioOutcome {
    let runtime =
        SimulationRuntimeGuard::enter(config.seed, TEST_EPOCH_MS, ProtectionProfile::ALL_ENABLED)
            .expect("churn simulation runtime must install");

    let weibull_config = WeibullChurnConfig::new(
        config.target_nodes,
        config.median_session_ms,
        config.graceful_leave_fraction,
    )
    .expect("scenario WeibullChurnConfig must be valid")
    .with_shape(config.shape_k)
    .expect("scenario Weibull shape must be valid")
    .with_rejoin_cooldown_ms(config.stabilize_interval_ms);

    let mut generator = WeibullSessionGenerator::new(config.seed, weibull_config);
    let schedule = generator.generate_schedule(config.duration_ms);

    let pool_size = schedule
        .iter()
        .map(|event| match event.action {
            SimAction::Join { node, via } => usize::from(node.0.max(via.0)).saturating_add(1),
            SimAction::Leave { node, .. } | SimAction::Crash { node } => {
                usize::from(node.0).saturating_add(1)
            }
            _ => 0,
        })
        .max()
        .unwrap_or(config.target_nodes)
        .max(config.target_nodes);

    let (nodes, app_inboxes) = build_node_pool(pool_size).await;
    let mut driver = ChurnTraceDriver::new(&nodes);

    let mut schedule_index = 0_usize;
    while let Some(event) = schedule.get(schedule_index) {
        if event.virtual_ms > 0 {
            break;
        }
        if let SimAction::Join { node, via } = event.action {
            let _ = driver.record_join(node, via);
        }
        schedule_index = schedule_index.saturating_add(1);
    }

    establish_initial_ring(&runtime, &nodes, config.target_nodes, &mut driver).await;
    seed_initial_storage(&runtime, &nodes, config.target_nodes, &mut driver).await;

    let mut timeline_points = BTreeSet::new();
    for event in schedule.iter().skip(schedule_index) {
        timeline_points.insert(event.virtual_ms);
    }
    let mut tick_ms = config.stabilize_interval_ms;
    while tick_ms <= config.duration_ms {
        timeline_points.insert(tick_ms);
        tick_ms = tick_ms.saturating_add(config.stabilize_interval_ms);
    }

    let mut pending_inbox_targets: BTreeSet<SimNodeId> = BTreeSet::new();
    let mut active_held_messages: BTreeMap<SimNodeId, Vec<u8>> = BTreeMap::new();
    let mut held_messages_injected = 0_u64;
    let mut held_messages_delivered = 0_u64;

    let mut sample_counters = LivenessSampleCounters::default();

    for target_ms in timeline_points {
        let current_ms = driver.state.virtual_ms();
        if target_ms > current_ms {
            let delta_ms = target_ms - current_ms;
            runtime
                .advance(Duration::from_millis(delta_ms))
                .await
                .expect("virtual clock must advance");
            driver.advance_virtual(delta_ms);
        }

        while let Some(event) = schedule.get(schedule_index) {
            if event.virtual_ms > target_ms {
                break;
            }
            let remaining = &schedule[schedule_index.saturating_add(1)..];
            execute_churn_event(
                &runtime,
                &nodes,
                &mut driver,
                event,
                remaining,
                &config,
                &mut pending_inbox_targets,
                &active_held_messages,
            )
            .await;
            schedule_index = schedule_index.saturating_add(1);
        }

        let is_stabilize_tick =
            target_ms > 0 && target_ms.is_multiple_of(config.stabilize_interval_ms);
        if is_stabilize_tick {
            clean_unavailable_across_live(&runtime, &nodes, &mut driver).await;

            sample_ring_liveness_and_lookups(&nodes, &driver, &mut sample_counters);

            run_stabilizer_round(&runtime, &nodes, &mut driver).await;

            inject_offline_inbox_messages(
                &runtime,
                &nodes,
                &mut driver,
                &mut pending_inbox_targets,
                &mut active_held_messages,
                &mut held_messages_injected,
            )
            .await;

            deliver_rejoined_inbox_messages(
                &runtime,
                &nodes,
                &app_inboxes,
                &mut driver,
                &mut active_held_messages,
                &mut held_messages_delivered,
            )
            .await;
        }
    }

    let orphan_samples = sample_counters.orphan_samples;
    let live_node_samples = sample_counters.live_node_samples;
    let lookup_successes = sample_counters.lookup_successes;
    let lookup_attempts = sample_counters.lookup_attempts;
    let population_sum = sample_counters.population_sum;
    let population_probes = sample_counters.population_probes;

    let duration_secs = (driver.state.virtual_ms().max(1) as f64) / 1_000.0;
    let mean_live_nodes = if population_probes > 0 {
        (population_sum as f64) / (population_probes as f64)
    } else {
        config.target_nodes as f64
    };
    let maintenance_bytes = driver.state.maintenance_admitted_bytes();
    let maintenance_bytes_per_node_sec =
        (maintenance_bytes as f64) / (mean_live_nodes.max(1.0) * duration_secs);

    let orphan_rate = if live_node_samples > 0 {
        (orphan_samples as f64) / (live_node_samples as f64)
    } else {
        0.0
    };
    let lookup_success_rate = if lookup_attempts > 0 {
        (lookup_successes as f64) / (lookup_attempts as f64)
    } else {
        1.0
    };
    let held_inbox_delivery_rate = if held_messages_injected > 0 {
        (held_messages_delivered as f64) / (held_messages_injected as f64)
    } else {
        1.0
    };

    let metrics = ChurnMetrics {
        orphan_samples,
        live_node_samples,
        orphan_rate,
        lookup_successes,
        lookup_attempts,
        lookup_success_rate,
        held_messages_injected,
        held_messages_delivered,
        held_inbox_delivery_rate,
        maintenance_bytes,
        maintenance_bytes_per_node_sec,
        mean_live_nodes,
        joins: driver.state.joins(),
        graceful_leaves: driver.state.graceful_leaves(),
        ungraceful_leaves: driver.state.ungraceful_leaves(),
        crashes: driver.state.crashes(),
    };

    let violations = driver.state.invariant_violations(CHURN_MODEL_LIMITS);
    teardown_all_nodes(&runtime, &nodes).await;
    drop(app_inboxes);
    drop(nodes);
    drop(runtime);

    ChurnScenarioOutcome {
        config,
        state: driver.state,
        metrics,
        violations,
    }
}

async fn build_node_pool(count: usize) -> (Vec<Node>, Vec<ApplicationInbox>) {
    let mut nodes = Vec::with_capacity(count);
    let mut inboxes = Vec::with_capacity(count);
    for index in 0..count {
        let node = prepare_node(deterministic_key(index)).await;
        let inbox = node
            .swarm
            .application_inbox()
            .expect("node application inbox must be claimable");
        nodes.push(node);
        inboxes.push(inbox);
    }
    (nodes, inboxes)
}

fn sorted_live_indices(nodes: &[Node], live: &BTreeSet<SimNodeId>) -> Vec<usize> {
    let mut indices: Vec<usize> = live.iter().map(|id| usize::from(id.0)).collect();
    indices.sort_by_key(|&idx| nodes.get(idx).map(Node::did));
    indices
}

async fn establish_initial_ring(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    initial_count: usize,
    driver: &mut ChurnTraceDriver,
) {
    let live: BTreeSet<SimNodeId> = (0..initial_count)
        .map(|idx| SimNodeId(idx as u16))
        .collect();
    let sorted = sorted_live_indices(nodes, &live);
    for pos in 0..sorted.len() {
        let left = sorted.get(pos).copied().unwrap_or(0);
        let right = sorted.get((pos + 1) % sorted.len()).copied().unwrap_or(0);
        if left == right {
            continue;
        }
        manually_establish_connection(&nodes[left].swarm, &nodes[right].swarm).await;
        drain_bootstrap_only(runtime, nodes).await;
        driver.sync_node_connections(nodes);
    }
    let live_dids: Vec<Did> = sorted.iter().map(|&idx| nodes[idx].did()).collect();
    for &idx in &sorted {
        let local_did = nodes[idx].did();
        if let Some(succ) = topology::successors(&live_dids, local_did, 1)
            .first()
            .copied()
        {
            nodes[idx]
                .dht()
                .admit_connected(succ, None)
                .expect("initial successor admit must succeed");
        }
        if let Some(pred) = topology::predecessor(&live_dids, local_did) {
            nodes[idx]
                .dht()
                .notify(pred)
                .expect("initial predecessor notify must succeed");
        }
        // Explicitly keep successor list at 1 entry (the immediate ring neighbor)
        // before stabilization so a graceful leave of that successor tests the
        // production `SuccessorRemoval::Preserve` vs `ReplaceWith` behavior.
        let immediate = topology::successors(&live_dids, local_did, 1);
        nodes[idx]
            .dht()
            .successors()
            .replace_state(&immediate)
            .expect("initial successor list must be replaceable");
    }
}

async fn seed_initial_storage(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    initial_count: usize,
    driver: &mut ChurnTraceDriver,
) {
    if initial_count < 2 {
        return;
    }
    let live: BTreeSet<SimNodeId> = (0..initial_count)
        .map(|idx| SimNodeId(idx as u16))
        .collect();
    let sorted = sorted_live_indices(nodes, &live);
    let entries_to_seed = sorted.len().min(4);
    let _ = driver.record_inject_sync(SimNodeId(0), entries_to_seed as u64);
    for slot in 0..entries_to_seed {
        let sender = sorted[slot];
        let receiver = sorted[(slot + 1) % sorted.len()];
        let receiver_did = nodes[receiver].did();
        let entry = derive_entry_for_owner(&nodes[receiver], slot);
        let msg = SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::AdditiveRepair,
            destination: StorageSyncDestination::PhysicalOwner(receiver_did),
            data: vec![entry],
        };
        let outcome = nodes[sender]
            .swarm
            .transport
            .send_storage_sync_tracked(msg)
            .await
            .expect("initial storage sync must succeed");
        assert!(matches!(outcome, StorageSyncOutcome::Sent(_)));
    }
    drain_churn_traced(runtime, nodes, driver).await;
}

fn derive_entry_for_owner(owner: &Node, slot: usize) -> PlacedEntry {
    for nonce in 0_u64..50_000 {
        let topic = format!("churn-seed-{slot}-{nonce}");
        let key = Entry::gen_did(&topic).expect("entry key must derive");
        if matches!(
            owner
                .dht()
                .find_storage_owner(key)
                .expect("storage owner lookup must succeed"),
            PeerRingAction::Some(_)
        ) {
            let payload = Bytes::from(vec![
                u8::try_from((nonce + slot as u64) % 251)
                    .expect("byte fits");
                INITIAL_STORAGE_PAYLOAD_BYTES
            ]);
            return PlacedEntry::new(key, live_entry(key, vec![payload], EntryKind::Data));
        }
    }
    let fallback_key = owner.did();
    let payload = Bytes::from(vec![1_u8; INITIAL_STORAGE_PAYLOAD_BYTES]);
    PlacedEntry::new(
        fallback_key,
        live_entry(fallback_key, vec![payload], EntryKind::Data),
    )
}

fn will_rejoin_at_stabilize_tick(
    node: SimNodeId,
    depart_ms: u64,
    remaining_schedule: &[ScheduledChurnEvent],
    stabilize_interval_ms: u64,
    duration_ms: u64,
) -> bool {
    let mut next_join_ms = None;
    let mut depart_after_join_ms = None;
    for event in remaining_schedule {
        match event.action {
            SimAction::Join { node: joined, .. } if joined == node && next_join_ms.is_none() => {
                next_join_ms = Some(event.virtual_ms);
            }
            SimAction::Leave { node: left, .. } | SimAction::Crash { node: left }
                if left == node && next_join_ms.is_some() =>
            {
                depart_after_join_ms = Some(event.virtual_ms);
                break;
            }
            _ => {}
        }
    }
    let Some(join_ms) = next_join_ms else {
        return false;
    };
    let inject_tick_ms = ((depart_ms / stabilize_interval_ms) + 1) * stabilize_interval_ms;
    if join_ms <= inject_tick_ms {
        return false;
    }
    let deliver_tick_ms = if join_ms > 0 && join_ms.is_multiple_of(stabilize_interval_ms) {
        join_ms
    } else {
        ((join_ms / stabilize_interval_ms) + 1) * stabilize_interval_ms
    };
    deliver_tick_ms <= duration_ms
        && depart_after_join_ms.is_none_or(|next_depart_ms| next_depart_ms > deliver_tick_ms)
}

#[allow(clippy::too_many_arguments)]
async fn execute_churn_event(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut ChurnTraceDriver,
    event: &ScheduledChurnEvent,
    remaining_schedule: &[ScheduledChurnEvent],
    config: &ChurnScenarioConfig,
    pending_inbox_targets: &mut BTreeSet<SimNodeId>,
    active_held_messages: &BTreeMap<SimNodeId, Vec<u8>>,
) {
    match event.action {
        SimAction::Leave {
            node,
            graceful: true,
        } => {
            let _ = driver.record_leave(node, true);
            let node_idx = usize::from(node.0);
            let connected_peers: Vec<usize> = (0..nodes.len())
                .filter(|&peer_idx| {
                    peer_idx != node_idx
                        && (nodes[node_idx]
                            .swarm
                            .transport
                            .get_connection(nodes[peer_idx].did())
                            .is_some()
                            || nodes[peer_idx]
                                .swarm
                                .transport
                                .get_connection(nodes[node_idx].did())
                                .is_some())
                })
                .collect();

            for &peer_idx in &connected_peers {
                let peer_did = nodes[peer_idx].did();
                if nodes[node_idx]
                    .swarm
                    .transport
                    .get_connection(peer_did)
                    .is_some()
                {
                    let _ = nodes[node_idx].swarm.disconnect(peer_did).await;
                }
                driver.close_edge_if_active(node, SimNodeId(peer_idx as u16));
            }
            clear_node_local_topology(&nodes[node_idx]);
            drain_churn_traced(runtime, nodes, driver).await;

            for &peer_idx in &connected_peers {
                let peer_id = SimNodeId(peer_idx as u16);
                if !nodes[peer_idx]
                    .swarm
                    .transport
                    .has_active_connection(nodes[node_idx].did())
                {
                    driver.observe_peer_removal_if_due(peer_id, node);
                }
            }
            if !active_held_messages.contains_key(&node)
                && will_rejoin_at_stabilize_tick(
                    node,
                    event.virtual_ms,
                    remaining_schedule,
                    config.stabilize_interval_ms,
                    config.duration_ms,
                )
            {
                pending_inbox_targets.insert(node);
            }
        }
        SimAction::Leave {
            node,
            graceful: false,
        }
        | SimAction::Crash { node } => {
            if matches!(event.action, SimAction::Crash { .. }) {
                let _ = driver.record_crash(node);
            } else {
                let _ = driver.record_leave(node, false);
            }
            let node_idx = usize::from(node.0);
            let node_did = nodes[node_idx].did();
            let mut remote_generations = BTreeSet::new();
            for (peer_idx, peer_node) in nodes.iter().enumerate() {
                if peer_idx == node_idx {
                    continue;
                }
                if let Some(conn) = peer_node.swarm.transport.get_connection(node_did) {
                    if let Ok(gen_id) = conn.dummy_generation_id() {
                        remote_generations.insert(gen_id);
                    }
                    let _ = peer_node
                        .swarm
                        .transport
                        .force_peer_connection_state_without_callback(
                            node_did,
                            WebrtcConnectionState::Closed,
                        );
                }
            }
            for peer_idx in 0..nodes.len() {
                if peer_idx == node_idx {
                    continue;
                }
                let peer_did = nodes[peer_idx].did();
                let _ = nodes[node_idx].swarm.disconnect(peer_did).await;
                driver.close_edge_if_active(node, SimNodeId(peer_idx as u16));
            }
            clear_node_local_topology(&nodes[node_idx]);
            discard_departed_close_callbacks(runtime, &remote_generations);
            drain_churn_traced(runtime, nodes, driver).await;
            if !active_held_messages.contains_key(&node)
                && will_rejoin_at_stabilize_tick(
                    node,
                    event.virtual_ms,
                    remaining_schedule,
                    config.stabilize_interval_ms,
                    config.duration_ms,
                )
            {
                pending_inbox_targets.insert(node);
            }
        }
        SimAction::Join { node, via } => {
            pending_inbox_targets.remove(&node);
            let node_idx = usize::from(node.0);
            let via_idx = usize::from(via.0);
            close_node_all_connections(runtime, nodes, node_idx, driver).await;
            clear_node_local_topology(&nodes[node_idx]);

            let _ = driver.record_join(node, via);
            if node_idx != via_idx {
                ensure_connected_pair(runtime, nodes, node_idx, via_idx, driver).await;
            }
            let live_dids: Vec<Did> = driver
                .state
                .live_nodes()
                .iter()
                .map(|id| nodes[usize::from(id.0)].did())
                .collect();
            let node_did = nodes[node_idx].did();
            if let Some(succ_did) = topology::successors(&live_dids, node_did, 1)
                .first()
                .copied()
            {
                if let Some(succ_idx) = nodes.iter().position(|n| n.did() == succ_did) {
                    if succ_idx != node_idx {
                        ensure_connected_pair(runtime, nodes, node_idx, succ_idx, driver).await;
                    }
                }
            }
            if let Some(pred_did) = topology::predecessor(&live_dids, node_did) {
                if let Some(pred_idx) = nodes.iter().position(|n| n.did() == pred_did) {
                    if pred_idx != node_idx {
                        ensure_connected_pair(runtime, nodes, pred_idx, node_idx, driver).await;
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn clear_node_local_topology(node: &Node) {
    let referenced: Vec<Did> = node
        .dht()
        .topology_state()
        .map(|state| state.referenced_peers().into_iter().collect())
        .unwrap_or_default();
    for peer in referenced {
        let _ = node.dht().remove(peer);
    }
}

fn discard_departed_close_callbacks(
    runtime: &SimulationRuntimeGuard,
    remote_generations: &BTreeSet<String>,
) {
    let pending = runtime
        .pending_deliveries()
        .expect("pending deliveries must classify");
    for delivery in pending {
        if delivery.class == ScheduledDeliveryClass::Lifecycle
            && remote_generations.contains(&delivery.connection_generation)
        {
            let _ = runtime.discard(&delivery);
        }
    }
}

pub(super) async fn close_node_all_connections(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    node_idx: usize,
    driver: &mut ChurnTraceDriver,
) {
    let node_id = SimNodeId(node_idx as u16);
    let node_did = nodes[node_idx].did();
    for peer_idx in 0..nodes.len() {
        if peer_idx == node_idx {
            continue;
        }
        let peer_id = SimNodeId(peer_idx as u16);
        let peer_did = nodes[peer_idx].did();
        let _ = nodes[node_idx].swarm.disconnect(peer_did).await;
        let _ = nodes[peer_idx].swarm.disconnect(node_did).await;
        driver.observe_peer_removal_if_due(peer_id, node_id);
        driver.close_edge_if_active(node_id, peer_id);
        driver.close_edge_if_active(peer_id, node_id);
    }
    drain_churn_traced(runtime, nodes, driver).await;
}

pub(super) async fn ensure_connected_pair(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    left_idx: usize,
    right_idx: usize,
    driver: &mut ChurnTraceDriver,
) {
    if left_idx == right_idx {
        return;
    }
    let left_did = nodes[left_idx].did();
    let right_did = nodes[right_idx].did();
    if nodes[left_idx]
        .swarm
        .transport
        .get_connection(right_did)
        .is_some()
        && nodes[right_idx]
            .swarm
            .transport
            .get_connection(left_did)
            .is_some()
    {
        driver.sync_node_connections(nodes);
        return;
    }
    let _ = nodes[left_idx].swarm.disconnect(right_did).await;
    let _ = nodes[right_idx].swarm.disconnect(left_did).await;
    driver.close_edge_if_active(SimNodeId(left_idx as u16), SimNodeId(right_idx as u16));
    driver.close_edge_if_active(SimNodeId(right_idx as u16), SimNodeId(left_idx as u16));
    drain_churn_traced(runtime, nodes, driver).await;
    let _ = nodes[left_idx].swarm.disconnect(right_did).await;
    let _ = nodes[right_idx].swarm.disconnect(left_did).await;
    manually_establish_connection(&nodes[left_idx].swarm, &nodes[right_idx].swarm).await;
    drain_churn_traced(runtime, nodes, driver).await;
    driver.sync_node_connections(nodes);
}

async fn clean_unavailable_across_live(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut ChurnTraceDriver,
) {
    let live_ids: Vec<SimNodeId> = driver.state.live_nodes().iter().copied().collect();
    let departed_ids: Vec<SimNodeId> = driver.state.departed_nodes().iter().copied().collect();
    for &observer in &live_ids {
        let observer_idx = usize::from(observer.0);
        for _ in 0..4 {
            let _ = nodes[observer_idx]
                .swarm
                .stabilizer()
                .clean_unavailable_connections()
                .await;
        }
        for &departed in &departed_ids {
            let departed_did = nodes[usize::from(departed.0)].did();
            if !nodes[observer_idx]
                .swarm
                .transport
                .has_active_connection(departed_did)
            {
                driver.observe_peer_removal_if_due(observer, departed);
            }
        }
    }
    drain_churn_traced(runtime, nodes, driver).await;
}

async fn run_stabilizer_round(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut ChurnTraceDriver,
) {
    let live_ids: Vec<SimNodeId> = driver.state.live_nodes().iter().copied().collect();

    for &node_id in &live_ids {
        let idx = usize::from(node_id.0);
        let _ = nodes[idx]
            .swarm
            .stabilizer()
            .converge_fingers_for_simulation()
            .await;
    }
    drain_churn_traced(runtime, nodes, driver).await;

    for &node_id in &live_ids {
        let idx = usize::from(node_id.0);
        let _ = nodes[idx]
            .swarm
            .stabilizer()
            .probe_peer_liveness_for_simulation()
            .await;
    }
    drain_churn_traced(runtime, nodes, driver).await;

    for &node_id in &live_ids {
        let idx = usize::from(node_id.0);
        let _ = nodes[idx].swarm.stabilizer().correct_stabilize().await;
    }
    drain_churn_traced(runtime, nodes, driver).await;

    // Ensure ring neighbors that have not yet finished in-band signaling are connected
    // so successor/predecessor handoff and storage repair progress cleanly.
    let sorted = sorted_live_indices(nodes, driver.state.live_nodes());
    if sorted.len() > 1 {
        let live_dids: Vec<Did> = sorted.iter().map(|&idx| nodes[idx].did()).collect();
        for pos in 0..sorted.len() {
            let left = sorted[pos];
            let right = sorted[(pos + 1) % sorted.len()];
            if left != right {
                ensure_connected_pair(runtime, nodes, left, right, driver).await;
            }
            let left_did = nodes[left].did();
            if let Some(pred) = topology::predecessor(&live_dids, left_did) {
                let _ = nodes[left].dht().notify(pred);
            }
        }
        for &idx in &sorted {
            let _ = nodes[idx].swarm.stabilizer().correct_stabilize().await;
        }
        drain_churn_traced(runtime, nodes, driver).await;
    }

    for &node_id in &live_ids {
        let idx = usize::from(node_id.0);
        nodes[idx].swarm.transport.request_storage_repair();
        let outcome = match nodes[idx]
            .swarm
            .stabilizer()
            .run_requested_storage_maintenance()
            .await
        {
            Some(StorageRepairOutcome::Complete) => SimMaintenanceOutcome::Complete,
            Some(StorageRepairOutcome::Deferred) => SimMaintenanceOutcome::Deferred,
            None => SimMaintenanceOutcome::Idle,
        };
        let cursor = nodes[idx]
            .swarm
            .transport
            .storage_repair_cursor_for_simulation()
            .expect("repair cursor must be readable");
        driver.observe_maintenance(node_id, outcome, cursor);
    }
    drain_churn_traced(runtime, nodes, driver).await;
}

async fn drain_bootstrap_only(runtime: &SimulationRuntimeGuard, nodes: &[Node]) {
    let mut quiet = 0;
    let mut in_flight_polls = 0;
    for _ in 0..MAX_DRAIN_STEPS {
        let pending = runtime
            .pending_deliveries()
            .expect("bootstrap deliveries must classify");
        if let Some(delivery) = pending
            .iter()
            .find(|item| item.class == ScheduledDeliveryClass::Lifecycle)
        {
            let _ = runtime
                .deliver(delivery)
                .await
                .expect("bootstrap lifecycle delivery must succeed");
            quiet = 0;
            in_flight_polls = 0;
        } else if let Some(first) = pending.first() {
            let _ = runtime
                .discard(first)
                .expect("bootstrap non-lifecycle discard must succeed");
            quiet = 0;
            in_flight_polls = 0;
        } else if nodes.iter().any(Node::in_flight) && in_flight_polls < MAX_IN_FLIGHT_POLLS {
            in_flight_polls += 1;
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= QUIESCENT_POLLS {
                return;
            }
        }
        settle_one_poll().await;
    }
    panic!("churn bootstrap did not quiesce within {MAX_DRAIN_STEPS} steps");
}

pub(super) async fn drain_churn_traced(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut ChurnTraceDriver,
) {
    let mut quiet = 0;
    let mut in_flight_polls = 0;
    for _ in 0..MAX_DRAIN_STEPS {
        let _ = runtime
            .new_pending_deliveries()
            .expect("new pending deliveries must classify");
        if let Some(delivery) = runtime
            .select_delivery(DeliveryStrategy::Seeded)
            .expect("queued churn delivery must classify")
        {
            let _ = runtime
                .deliver(&delivery)
                .await
                .expect("churn delivery must remain stable");
            driver.record_delivered_frame(runtime, nodes, &delivery);
            let service_ms = frame_service_ms(delivery.bytes);
            runtime
                .advance(Duration::from_millis(service_ms))
                .await
                .expect("virtual clock must advance for frame service");
            driver.advance_virtual(service_ms);
            quiet = 0;
            in_flight_polls = 0;
        } else if nodes.iter().any(Node::in_flight) && in_flight_polls < MAX_IN_FLIGHT_POLLS {
            driver.drain_production_observations(runtime);
            in_flight_polls += 1;
            quiet = 0;
        } else {
            driver.drain_production_observations(runtime);
            quiet += 1;
            if quiet >= QUIESCENT_POLLS {
                driver.sync_node_connections(nodes);
                return;
            }
        }
        settle_one_poll().await;
    }
    panic!("churn traced network did not quiesce within {MAX_DRAIN_STEPS} steps");
}

fn frame_service_ms(bytes: usize) -> u64 {
    u64::try_from(bytes.max(1).div_ceil(VIRTUAL_SERVICE_BYTES_PER_MS))
        .expect("frame service cost must fit u64")
        .min(MAX_FRAME_SERVICE_MS)
}

async fn settle_one_poll() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

async fn teardown_all_nodes(runtime: &SimulationRuntimeGuard, nodes: &[Node]) {
    let mut generations: Vec<String> = Vec::new();
    for left in 0..nodes.len() {
        for right in 0..nodes.len() {
            if left == right {
                continue;
            }
            if let Some(conn) = nodes[left]
                .swarm
                .transport
                .get_connection(nodes[right].did())
            {
                if let Ok(gen_id) = conn.dummy_generation_id() {
                    generations.push(gen_id);
                }
            }
        }
    }
    for node in nodes {
        let _ = node.swarm.close().await;
    }
    let mut in_flight_polls = 0;
    for _ in 0..MAX_DRAIN_STEPS {
        if let Some(delivery) = runtime
            .pending_deliveries()
            .expect("teardown deliveries must classify")
            .first()
        {
            let _ = runtime.deliver(delivery).await;
            in_flight_polls = 0;
        } else if nodes.iter().any(Node::in_flight) && in_flight_polls < MAX_IN_FLIGHT_POLLS {
            in_flight_polls += 1;
        } else {
            break;
        }
        settle_one_poll().await;
    }
    for gen_id in generations {
        assert!(
            !dummy_controlled::is_connection_registered(&gen_id),
            "dummy connection generation leaked after churn scenario teardown: {gen_id}"
        );
    }
}
