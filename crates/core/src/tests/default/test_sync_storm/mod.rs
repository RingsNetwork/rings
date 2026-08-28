//! Deterministic multi-node sync-storm scenarios for issue #686.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use rings_transport::connections::dummy_controlled;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::Chord;
use crate::dht::PeerRingAction;
use crate::dht::StorageRepairOutcome;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::fair_admission::retained_wire_bytes;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::PeerLivenessProbe;
use crate::message::SyncEntriesWithSuccessor;
use crate::session::SessionSk;
use crate::simulation::model::SimAction;
use crate::simulation::model::SimConnectionState;
use crate::simulation::model::SimEventId;
use crate::simulation::model::SimFrameIdentity;
use crate::simulation::model::SimInvariantViolation;
use crate::simulation::model::SimLimits;
use crate::simulation::model::SimMaintenanceOutcome;
use crate::simulation::model::SimNodeId;
use crate::simulation::model::SimState;
use crate::simulation::model::SimTransferClass;
use crate::simulation::DeliveryStrategy;
use crate::simulation::ProductionCapacityObservations;
use crate::simulation::ProtectionLayer;
use crate::simulation::ProtectionObservations;
use crate::simulation::ProtectionProfile;
use crate::simulation::ScheduledDelivery;
use crate::simulation::ScheduledDeliveryClass;
use crate::simulation::SimulationRuntimeGuard;
use crate::simulation::CONTROL_DEADLINE_MS;
use crate::storage::MemStorage;
use crate::swarm::transport::outbound_submit_count_for_test;
use crate::swarm::transport::TrackedStorageSyncOutcome;
use crate::swarm::transport::OUTBOUND_CONTROL_BURST;
use crate::swarm::transport::OUTBOUND_GLOBAL_BYTE_CAPACITY;
use crate::swarm::transport::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
use crate::swarm::transport::PEER_LIVENESS_TIMEOUT_MS;
use crate::swarm::SwarmBuilder;
use crate::tests::default::prepare_node;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

const TEST_EPOCH_MS: u128 = 1_700_000_000_000;
const CHUNK_MESSAGE_SIZE: usize = 8 * 1024;
const ENTRY_PAYLOAD_BYTES: usize = 8 * 1024;
const MAX_DRAIN_STEPS: usize = 100_000;
const QUIESCENT_POLLS: usize = 8;
const VIRTUAL_SERVICE_BYTES_PER_MS: usize = 256;
const MAX_FRAME_SERVICE_MS: u64 = 64;

const MODEL_LIMITS: SimLimits = SimLimits {
    node_bytes: 128 * 1024 * 1024,
    global_bytes: 256 * 1024 * 1024,
    control_deadline_ms: CONTROL_DEADLINE_MS,
    repair_amplification: 1,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScenarioTopology {
    Ring,
    Hotspot,
}

impl ScenarioTopology {
    const fn name(self) -> &'static str {
        match self {
            Self::Ring => "ring",
            Self::Hotspot => "hotspot",
        }
    }
}

struct ScenarioOutcome {
    state: SimState,
    persisted_entries: usize,
    expected_entries: usize,
    count: usize,
    topology: ScenarioTopology,
    seed: u64,
    profile: ProtectionProfile,
    strategy: DeliveryStrategy,
    protection_violations: BTreeSet<ProtectionLayer>,
    protection_observations: ProtectionObservations,
    capacity_observations: ProductionCapacityObservations,
    pressure_snapshot: serde_json::Value,
    recovery_elapsed_ms: u128,
    overload_witness: &'static str,
}

impl ScenarioOutcome {
    fn replay_command(&self) -> String {
        scenario_replay_command(
            self.count,
            self.topology,
            self.seed,
            self.profile,
            self.strategy,
        )
    }

    fn diagnostic(&self) -> String {
        let events = self.state.trace().events();
        let recent = &events[events.len().saturating_sub(8)..];
        let digest = self
            .state
            .trace()
            .digest()
            .map(hex::encode)
            .unwrap_or_else(|error| format!("trace-serialization-error:{error}"));
        format!(
            "scenario={} n={} seed={} profile={} strategy={} recovery_elapsed_ms={} virtual_state={:?} \
             trace_digest={} recent_events={recent:?} replay=`{}`",
            self.topology.name(),
            self.count,
            self.seed,
            self.profile.name(),
            self.strategy.name(),
            self.recovery_elapsed_ms,
            self.state.snapshot(),
            digest,
            self.replay_command(),
        )
    }

    fn canonical_replay_json(&self) -> Vec<u8> {
        let trace = self
            .state
            .trace()
            .canonical_json()
            .expect("scenario trace must serialize");
        serde_json::to_vec(&serde_json::json!({
            "trace": serde_json::from_slice::<serde_json::Value>(&trace)
                .expect("canonical trace must decode"),
            "pressure_snapshot": self.pressure_snapshot,
            "protection_observations": self.protection_observations,
            "capacity_observations": self.capacity_observations,
            "persisted_entries": self.persisted_entries,
            "expected_entries": self.expected_entries,
            "recovery_elapsed_ms": self.recovery_elapsed_ms,
            "overload_witness": self.overload_witness,
        }))
        .expect("complete replay witness must serialize")
    }
}

fn scenario_replay_command(
    count: usize,
    topology: ScenarioTopology,
    seed: u64,
    profile: ProtectionProfile,
    strategy: DeliveryStrategy,
) -> String {
    format!(
        "SYNC_STORM_SEED={seed} SYNC_STORM_N={count} SYNC_STORM_TOPOLOGY={} \
         SYNC_STORM_PROFILE={} SYNC_STORM_STRATEGY={} cargo test -p rings-core \
         --features dummy --no-default-features test_replay_sync_storm_from_env \
         -- --ignored --nocapture",
        topology.name(),
        profile.name(),
        strategy.name(),
    )
}

fn typed_overload_witness(error: &Error) -> &'static str {
    match error {
        Error::OutboundTransferCapacityExceeded { .. } => "OutboundTransferCapacityExceeded",
        Error::OutboundTransferMemoryCapacityExceeded { .. } => {
            "OutboundTransferMemoryCapacityExceeded"
        }
        other => panic!("class pressure returned a non-overload error: {other:?}"),
    }
}

fn node_ids(nodes: &[Node]) -> BTreeMap<String, SimNodeId> {
    nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            (
                node.did().to_string(),
                SimNodeId(u16::try_from(index).expect("node index must fit trace id")),
            )
        })
        .collect()
}

fn connection_endpoints(nodes: &[Node]) -> BTreeMap<String, (String, String)> {
    let mut endpoints = BTreeMap::new();
    for local in nodes {
        for peer in nodes {
            if local.did() == peer.did() {
                continue;
            }
            let Some(connection) = local.swarm.transport.get_connection(peer.did()) else {
                continue;
            };
            let generation = connection
                .dummy_generation_id()
                .expect("dummy generation must remain live while the scenario runs");
            assert!(endpoints
                .insert(
                    generation,
                    (local.did().to_string(), peer.did().to_string()),
                )
                .is_none());
        }
    }
    endpoints
}

async fn close_nodes(runtime: &SimulationRuntimeGuard, nodes: &[Node], generations: &[String]) {
    for left in 0..nodes.len() {
        for right in left + 1..nodes.len() {
            let peer = nodes[right].did();
            if nodes[left].swarm.transport.get_connection(peer).is_some() {
                nodes[left]
                    .swarm
                    .disconnect(peer)
                    .await
                    .expect("scenario connection must close cleanly");
            }
            if nodes[right]
                .swarm
                .transport
                .get_connection(nodes[left].did())
                .is_some()
            {
                nodes[right]
                    .swarm
                    .disconnect(nodes[left].did())
                    .await
                    .expect("remaining remote generation must close cleanly");
            }
        }
    }
    drain_teardown(runtime, nodes).await;
    assert!(nodes.iter().all(|node| !node.has_handshaking_connection()));
    for generation in generations {
        assert!(
            !dummy_controlled::is_connection_registered(generation),
            "dummy connection generation leaked after scenario teardown: {generation}"
        );
    }
}

async fn drain_teardown(runtime: &SimulationRuntimeGuard, nodes: &[Node]) {
    for _ in 0..MAX_DRAIN_STEPS {
        if let Some(delivery) = runtime
            .pending_deliveries()
            .expect("teardown event must classify")
            .first()
        {
            // Closing unregisters the exact dummy generation before its queued
            // terminal callback is observed. `deliver` still removes that
            // orphaned callback and reports false, which is valid teardown.
            let _ = runtime
                .deliver(delivery)
                .await
                .expect("teardown delivery must remain stable");
        } else if !network_busy(nodes) {
            return;
        }
        settle_one_poll().await;
    }
    panic!("scenario teardown did not quiesce within the harness bound");
}

fn model_class(class: ScheduledDeliveryClass) -> Option<SimTransferClass> {
    match class {
        ScheduledDeliveryClass::Lifecycle => None,
        ScheduledDeliveryClass::Control => Some(SimTransferClass::Control),
        ScheduledDeliveryClass::Storage => Some(SimTransferClass::Storage),
        ScheduledDeliveryClass::Reassembly => Some(SimTransferClass::Reassembly),
        ScheduledDeliveryClass::E2e => Some(SimTransferClass::E2e),
        ScheduledDeliveryClass::Application => Some(SimTransferClass::Application),
    }
}

fn deterministic_key(index: usize) -> SecretKey {
    let mut bytes = [0_u8; 32];
    let scalar = u64::try_from(index)
        .expect("node index must fit u64")
        .saturating_add(1);
    bytes[24..].copy_from_slice(&scalar.to_be_bytes());
    SecretKey::from_bytes(bytes).expect("positive test scalar must be a valid secret key")
}

async fn build_nodes(count: usize) -> Vec<Node> {
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        nodes.push(prepare_node(deterministic_key(index)).await);
    }
    nodes
}

fn build_repair_nodes(count: usize) -> Vec<Node> {
    (0..count)
        .map(|index| {
            let session = SessionSk::new_with_seckey(&deterministic_key(index))
                .expect("deterministic repair-node session must be valid");
            let swarm = SwarmBuilder::new(
                0,
                "stun://stun.l.google.com:19302",
                Box::new(MemStorage::new()),
                session,
            )
            .dht_storage_redundancy(2)
            .dht_virtual_nodes(0)
            .build();
            Node::new(Arc::new(swarm))
        })
        .collect()
}

fn sorted_indices(nodes: &[Node]) -> Vec<usize> {
    let mut indices = (0..nodes.len()).collect::<Vec<_>>();
    indices.sort_by_key(|index| nodes[*index].did());
    indices
}

fn physical_edges(nodes: &[Node], kind: ScenarioTopology) -> Vec<(usize, usize)> {
    let sorted = sorted_indices(nodes);
    let mut edges = BTreeSet::new();
    match kind {
        ScenarioTopology::Ring => {
            for position in 0..sorted.len() {
                let left = sorted[position];
                let right = sorted[(position + 1) % sorted.len()];
                edges.insert(ordered_edge(left, right));
            }
        }
        ScenarioTopology::Hotspot => {
            let center = sorted[0];
            for &leaf in sorted.iter().skip(1) {
                edges.insert(ordered_edge(center, leaf));
            }
        }
    }
    edges.into_iter().collect()
}

const fn ordered_edge(left: usize, right: usize) -> (usize, usize) {
    if left < right {
        (left, right)
    } else {
        (right, left)
    }
}

async fn establish_topology(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
) {
    for (left, right) in physical_edges(nodes, kind) {
        manually_establish_connection(&nodes[left].swarm, &nodes[right].swarm).await;
        drain_bootstrap(runtime, nodes).await;
    }
}

fn install_chord_view(nodes: &[Node], kind: ScenarioTopology) {
    let dids = nodes.iter().map(Node::did).collect::<Vec<_>>();
    if matches!(kind, ScenarioTopology::Hotspot) {
        install_hotspot_chord_view(nodes, &dids);
        return;
    }
    for node in nodes {
        for &did in &dids {
            if did != node.did() {
                node.dht().join(did).expect("test Chord join must succeed");
                node.dht()
                    .notify(did)
                    .expect("test Chord notify must succeed");
            }
        }
    }
}

fn install_hotspot_chord_view(nodes: &[Node], dids: &[crate::dht::Did]) {
    let center_index = sorted_indices(nodes)[0];
    let center = nodes[center_index].did();
    for &did in dids {
        if did != center {
            nodes[center_index]
                .dht()
                .join(did)
                .expect("hotspot center must know every leaf");
            nodes[center_index]
                .dht()
                .notify(did)
                .expect("hotspot center predecessor view must update");
        }
    }
    for (index, node) in nodes.iter().enumerate() {
        if index == center_index {
            continue;
        }
        for &did in dids {
            if did != node.did() {
                let _ = node.dht().remove(did);
            }
        }
        node.dht()
            .join(center)
            .expect("hotspot leaf must route through center");
        node.dht()
            .notify(center)
            .expect("hotspot leaf predecessor view must update");
    }
}

fn workload_edges(nodes: &[Node], kind: ScenarioTopology) -> Vec<(usize, usize)> {
    let sorted = sorted_indices(nodes);
    match kind {
        ScenarioTopology::Ring => (0..sorted.len())
            .map(|position| (sorted[position], sorted[(position + 1) % sorted.len()]))
            .collect(),
        ScenarioTopology::Hotspot => {
            let center = sorted[0];
            sorted.iter().skip(1).map(|&leaf| (leaf, center)).collect()
        }
    }
}

fn entry_owned_by(owner: &Node, label: &str) -> PlacedEntry {
    for nonce in 0_u64..100_000 {
        let topic = format!("sync-storm-{label}-{nonce}");
        let key = Entry::gen_did(&topic).expect("test entry DID must derive");
        let action = owner
            .dht()
            .find_storage_owner(key)
            .expect("test owner lookup must succeed");
        if matches!(action, PeerRingAction::Some(_)) {
            let data = vec![u8::try_from(nonce % 251).expect("byte must fit"); ENTRY_PAYLOAD_BYTES]
                .encode()
                .expect("test payload must encode");
            return PlacedEntry::new(key, Entry::new(key, vec![data], EntryKind::Data));
        }
    }
    panic!("failed to derive an entry owned by {}", owner.did());
}

async fn submit_workload(nodes: &[Node], kind: ScenarioTopology) -> usize {
    let edges = workload_edges(nodes, kind);
    for (job, &(sender, receiver)) in edges.iter().enumerate() {
        let receiver_did = nodes[receiver].did();
        let entry = entry_owned_by(&nodes[receiver], &job.to_string());
        let owner_action = nodes[receiver]
            .dht()
            .find_storage_owner(entry.key)
            .expect("receiver must resolve storage ownership");
        assert!(
            matches!(owner_action, PeerRingAction::Some(_)),
            "receiver does not own generated placement: {owner_action:?}"
        );
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
            .expect("real storage sync submission must succeed");
        assert!(
            matches!(outcome, TrackedStorageSyncOutcome::Delivered(_)),
            "storm submission must be remotely admitted: {outcome:?}"
        );
        nodes[sender]
            .swarm
            .send_direct_message(
                Message::PeerLivenessProbe(PeerLivenessProbe {
                    sent_at_ms: i64::try_from(TEST_EPOCH_MS).expect("epoch must fit i64"),
                }),
                receiver_did,
            )
            .await
            .expect("control probe must enter the real outbound scheduler");
    }
    edges.len()
}

fn network_busy(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| {
        node.has_handshaking_connection()
            || node.has_outbound_transfer()
            || node.has_inbound_message()
    })
}

fn assert_healthy_connections(nodes: &[Node], kind: ScenarioTopology) {
    for (left, right) in physical_edges(nodes, kind) {
        assert!(nodes[left]
            .swarm
            .transport
            .get_connection(nodes[right].did())
            .is_some());
        assert!(nodes[right]
            .swarm
            .transport
            .get_connection(nodes[left].did())
            .is_some());
    }
}

async fn settle_one_poll() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

async fn drain_bootstrap(runtime: &SimulationRuntimeGuard, nodes: &[Node]) {
    let mut quiet = 0;
    for _ in 0..MAX_DRAIN_STEPS {
        let pending = runtime
            .pending_deliveries()
            .expect("bootstrap events must classify");
        if let Some(delivery) = pending
            .iter()
            .find(|delivery| delivery.class == ScheduledDeliveryClass::Lifecycle)
        {
            assert!(runtime
                .deliver(delivery)
                .await
                .expect("bootstrap delivery must remain stable"));
            quiet = 0;
        } else if !pending.is_empty() {
            assert!(runtime
                .discard(&pending[0])
                .expect("bootstrap discard must remain stable"));
            quiet = 0;
        } else if network_busy(nodes) {
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= QUIESCENT_POLLS {
                return;
            }
        }
        settle_one_poll().await;
    }
    panic!("dummy network did not quiesce within {MAX_DRAIN_STEPS} steps");
}

async fn drain_untraced(runtime: &SimulationRuntimeGuard, nodes: &[Node]) {
    let mut quiet = 0;
    for _ in 0..MAX_DRAIN_STEPS {
        if let Some(delivery) = runtime
            .select_delivery(DeliveryStrategy::Fifo)
            .expect("untraced event must classify")
        {
            assert!(runtime
                .deliver(&delivery)
                .await
                .expect("untraced delivery must remain stable"));
            quiet = 0;
        } else if network_busy(nodes) {
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= QUIESCENT_POLLS {
                return;
            }
        }
        settle_one_poll().await;
    }
    panic!("untraced production workload did not quiesce within the harness bound");
}

async fn drain_traced(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    strategy: DeliveryStrategy,
    driver: &mut TraceDriver,
) {
    let mut quiet = 0;
    for step in 0..MAX_DRAIN_STEPS {
        if step == 0 || step.is_power_of_two() {
            persist_inflight_trace_artifact("drain", runtime, &driver.state)
                .expect("in-flight trace artifact must be writable");
        }
        let newly_pending = runtime
            .new_pending_deliveries()
            .expect("new queued events must classify once");
        driver.observe_pending(runtime, &newly_pending);
        if let Some(delivery) = runtime
            .select_delivery(strategy)
            .expect("queued event must classify")
        {
            assert!(runtime
                .deliver(&delivery)
                .await
                .expect("traced delivery must remain stable"));
            driver.observe_delivery(runtime, &delivery);
            driver.observe_production_handlers(runtime);
            let service_ms = frame_service_ms(delivery.bytes);
            runtime
                .advance(Duration::from_millis(service_ms))
                .await
                .expect("virtual clock must advance");
            driver.advance_virtual(service_ms);
            quiet = 0;
        } else if network_busy(nodes) {
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= QUIESCENT_POLLS {
                return;
            }
        }
        settle_one_poll().await;
    }
    persist_inflight_trace_artifact("drain-limit", runtime, &driver.state)
        .expect("bounded drain failure artifact must be writable");
    panic!("traced network did not quiesce within {MAX_DRAIN_STEPS} steps");
}

fn frame_service_ms(bytes: usize) -> u64 {
    u64::try_from(bytes.max(1).div_ceil(VIRTUAL_SERVICE_BYTES_PER_MS))
        .expect("frame service cost must fit virtual time")
        .min(MAX_FRAME_SERVICE_MS)
}

fn recovery_bound_ms(expected_entries: usize, active_peer_queues: usize) -> u128 {
    // A storm work item produces one control request plus bounded storage,
    // chunk, report, and acknowledgement frames. Reserving a quarter of the
    // negotiated dummy MTU for payload gives a conservative frame count even
    // after production envelope overhead.
    let minimum_chunk_data = (CHUNK_MESSAGE_SIZE / 4).max(1);
    let data_frames = ENTRY_PAYLOAD_BYTES.div_ceil(minimum_chunk_data);
    let frames_per_entry = data_frames.saturating_add(4);
    let workload_frames = expected_entries.saturating_mul(frames_per_entry);
    // Every live peer scheduler owns the production per-peer transfer bound.
    // The production global retained-byte budget independently caps the same
    // carry-over; using a one-byte minimum wire frame is deliberately
    // conservative while keeping the derivation tied to the real global gate.
    let peer_capacity_frames = active_peer_queues.saturating_mul(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    let global_capacity_frames = OUTBOUND_GLOBAL_BYTE_CAPACITY / retained_wire_bytes(1).max(1);
    let admitted_carry_over = peer_capacity_frames.min(global_capacity_frames);
    let drainable_frames = workload_frames.saturating_add(admitted_carry_over);
    u128::try_from(drainable_frames.saturating_mul(MAX_FRAME_SERVICE_MS as usize))
        .expect("derived recovery bound must fit virtual time")
}

async fn count_persisted(nodes: &[Node]) -> Vec<usize> {
    let mut counts = Vec::with_capacity(nodes.len());
    for node in nodes {
        let node_count = node
            .dht()
            .storage
            .count()
            .await
            .expect("memory storage count must succeed");
        counts.push(usize::try_from(node_count).expect("memory storage count must fit usize"));
    }
    counts
}

async fn run_empty_repair_maintenance(nodes: &[Node], driver: &mut TraceDriver) {
    for node in nodes {
        node.swarm
            .stabilizer()
            .clean_unavailable_connections()
            .await
            .expect("pre-storm topology maintenance must succeed");
    }
    for (index, node) in nodes.iter().enumerate() {
        node.swarm.transport.request_storage_repair();
        let outcome = node
            .swarm
            .stabilizer()
            .run_requested_storage_repair()
            .await
            .expect("claimed empty repair request must run");
        let model_outcome = match outcome {
            StorageRepairOutcome::Complete => SimMaintenanceOutcome::Complete,
            StorageRepairOutcome::Deferred => SimMaintenanceOutcome::Deferred,
        };
        let cursor = node
            .swarm
            .transport
            .storage_repair_cursor_for_simulation()
            .expect("repair cursor must remain observable");
        driver.observe_maintenance(index, model_outcome, cursor);
    }
}

async fn begin_liveness_under_storm(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut TraceDriver,
) -> BTreeMap<(String, String), uuid::Uuid> {
    for node in nodes {
        node.swarm
            .stabilizer()
            .probe_peer_liveness_for_simulation()
            .await
            .expect("real liveness probe pass must succeed");
    }
    let probes = runtime
        .new_pending_deliveries()
        .expect("real liveness probes must classify");
    driver.observe_pending(runtime, &probes);
    let control_probe_count = probes
        .iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Control)
        .count();
    assert_eq!(
        control_probe_count,
        driver.endpoints.len(),
        "the post-maintenance watermark must isolate exactly one Stabilizer probe per generation"
    );
    let mut identities = BTreeMap::new();
    for probe in probes
        .iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Control)
    {
        let transaction = probe
            .transaction_id
            .expect("real liveness probe must retain its transaction identity");
        let (receiver, sender) = driver
            .endpoints
            .get(&probe.connection_generation)
            .expect("liveness generation must belong to the scenario");
        identities.insert((sender.clone(), receiver.clone()), transaction);
    }
    assert_eq!(
        identities.len(),
        driver.endpoints.len(),
        "real Stabilizer must probe every healthy connection generation"
    );
    identities
}

async fn conclude_healthy_liveness(
    nodes: &[Node],
    expected: &BTreeMap<String, (String, String)>,
    probes: &BTreeMap<(String, String), uuid::Uuid>,
    driver: &mut TraceDriver,
) {
    for node in nodes {
        node.swarm
            .stabilizer()
            .clean_unavailable_connections()
            .await
            .expect("real healthy liveness verdict must succeed");
    }
    let current = connection_endpoints(nodes);
    driver.observe_liveness(&current, probes);
    assert_eq!(
        &current, expected,
        "healthy probe/report traffic must preserve every connection generation"
    );
}

fn assert_enabled_outcome(outcome: &ScenarioOutcome) {
    let diagnostic = outcome.diagnostic();
    let snapshot = outcome.state.snapshot();
    assert!(
        outcome.capacity_observations.validate().is_ok(),
        "{}; {diagnostic}",
        outcome
            .capacity_observations
            .validate()
            .expect_err("failed production capacity validation must explain itself")
    );
    assert!(
        outcome.protection_violations.is_empty(),
        "enabled profile violated production layers: {:?}; {diagnostic}",
        outcome.protection_violations,
    );
    assert_eq!(
        outcome.persisted_entries,
        outcome.expected_entries,
        "deliveries: control={}, storage={}, reassembly={}, application={}, events={}; {diagnostic}",
        outcome.state.class_deliveries(SimTransferClass::Control),
        outcome.state.class_deliveries(SimTransferClass::Storage),
        outcome.state.class_deliveries(SimTransferClass::Reassembly),
        outcome
            .state
            .class_deliveries(SimTransferClass::Application),
        outcome.state.trace().events().len(),
    );
    assert!(
        outcome.state.invariant_violations(MODEL_LIMITS).is_empty(),
        "{diagnostic}"
    );
    assert!(
        outcome.state.class_deliveries(SimTransferClass::Control) > 0,
        "{diagnostic}"
    );
    assert!(
        outcome.state.class_deliveries(SimTransferClass::Reassembly) > 0,
        "{diagnostic}"
    );
    assert!(snapshot.connection_generations > 0, "{diagnostic}");
    assert_eq!(snapshot.liveness_verdicts, snapshot.connection_generations);
    assert_eq!(snapshot.liveness_removals, 0, "{diagnostic}");
    assert!(snapshot.reassembly_advances > 0, "{diagnostic}");
    assert!(snapshot.reassembly_barriers > 0, "{diagnostic}");
    assert_eq!(snapshot.blocked_control_barriers, 0, "{diagnostic}");
    assert!(snapshot.actor_yields > 0, "{diagnostic}");
    assert_eq!(snapshot.maintenance_runs, outcome.count as u64);
    assert!(snapshot.storm_stopped, "{diagnostic}");
}

mod artifacts;
mod cases;
mod legacy;
mod pressure;
mod scenario;
mod trace;

use artifacts::*;
use legacy::legacy_feedback_loop_state;
use pressure::exercise_barrier_control_exemption;
use pressure::exercise_bounded_control_burst;
use pressure::exercise_per_entry_yield;
use scenario::run_scenario;
use trace::TraceDriver;
