use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use super::prepare_node;
use super::prepare_repair_node;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::maintenance_phase_trace_for_test;
use crate::dht::reset_maintenance_phase_trace_for_test;
use crate::dht::successor::SuccessorWriter;
use crate::dht::topology;
use crate::dht::MaintenancePhaseEvent;
use crate::dht::MaintenancePhaseKind;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::lifecycle::StopSource;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessageClass;
use crate::message::NotifyPredecessorSend;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::Transport;
use crate::swarm::Swarm;
use crate::tests::assert_control_interleaves_transfer;
use crate::tests::control_interleaves_transfer;
use crate::tests::manually_establish_connection;
use crate::tests::midpoint_storage_key;
use crate::tests::multi_frame_storage_sync_entries;
use crate::tests::replace_observed_fingers;
use crate::tests::ring_topology_converged;
use crate::tests::tail_storage_key;
use crate::utils::get_epoch_ms_i64;
use crate::utils::sleep;

const SOAK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const SOAK_POLL_ATTEMPTS: usize = 400;
const REPAIR_POLL_ATTEMPTS: usize = 1_200;
const BROWSER_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(500);
const BROWSER_REPAIR_SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
const BROWSER_PHASE_TRACE_TIMEOUT: Duration = Duration::from_secs(15);

async fn wait_for_full_mesh(nodes: &[&crate::swarm::Swarm]) {
    for _ in 0..SOAK_POLL_ATTEMPTS {
        let connected = nodes.iter().all(|node| {
            nodes
                .iter()
                .all(|peer| node.did() == peer.did() || node.peer_dids().contains(&peer.did()))
        });
        if connected {
            return;
        }
        sleep(SOAK_POLL_INTERVAL).await;
    }
    panic!("three-node browser mesh did not become routable");
}

async fn wait_for_ring_convergence(nodes: &[&crate::swarm::Swarm]) {
    for _ in 0..SOAK_POLL_ATTEMPTS {
        if ring_topology_converged(nodes).unwrap() {
            return;
        }
        sleep(SOAK_POLL_INTERVAL).await;
    }
    panic!("three-node browser ring did not converge");
}

async fn run_stabilization_round(nodes: &[&crate::swarm::Swarm; 3]) {
    let [first_node, second_node, third_node] = *nodes;
    let first = first_node.stabilizer();
    let second = second_node.stabilizer();
    let third = third_node.stabilizer();
    let (first, second, third) =
        futures::join!(first.stabilize(), second.stabilize(), third.stabilize(),);
    first.unwrap();
    second.unwrap();
    third.unwrap();
    sleep(SOAK_POLL_INTERVAL).await;
}

async fn storage_matches(
    node: &crate::swarm::Swarm,
    key: crate::dht::Did,
    expected: &Entry,
) -> bool {
    node.dht()
        .storage
        .get(&key.to_string())
        .await
        .unwrap()
        .as_ref()
        == Some(expected)
}

struct RepairPlacement {
    owner: crate::dht::Did,
    key: crate::dht::Did,
    expected: Entry,
}

struct RepairFixture {
    placements: [RepairPlacement; 2],
}

async fn prepare_repair_mesh() -> [Arc<Swarm>; 3] {
    let nodes = [
        prepare_repair_node(SecretKey::random()).await,
        prepare_repair_node(SecretKey::random()).await,
        prepare_repair_node(SecretKey::random()).await,
    ];
    let [node1, node2, node3] = &nodes;
    manually_establish_connection(node1, node2).await;
    manually_establish_connection(node1, node3).await;
    manually_establish_connection(node2, node3).await;
    let swarms = [node1.as_ref(), node2.as_ref(), node3.as_ref()];
    wait_for_full_mesh(&swarms).await;
    for node in swarms {
        let peers = swarms
            .iter()
            .filter(|peer| peer.did() != node.did())
            .map(|peer| peer.did())
            .collect::<Vec<_>>();
        node.dht().successors().extend(&peers).unwrap();
    }
    for _ in 0..8 {
        run_stabilization_round(&swarms).await;
        if ring_topology_converged(&swarms).unwrap() {
            break;
        }
    }
    wait_for_ring_convergence(&swarms).await;
    for node in swarms {
        for peer in swarms.iter().filter(|peer| peer.did() != node.did()) {
            node.transport
                .force_peer_connected_at(peer.did(), get_epoch_ms_i64() - 31_000)
                .unwrap();
        }
    }
    nodes
}

async fn seed_remote_repair_entries(node1: &Swarm, node2: &Swarm, node3: &Swarm) -> RepairFixture {
    let mut routed_peers = [node2, node3];
    routed_peers.sort_by_key(|node| topology::dist(node1.did(), node.did()));
    let [head, tail] = routed_peers;
    replace_observed_fingers(node1, &[(0, head.did()), (3, tail.did())]).unwrap();
    let head_key = midpoint_storage_key(node1.did(), head.did(), tail.did());
    let tail_key = tail_storage_key(node1.did(), tail.did());
    let head_entry = Entry::new(
        head_key,
        vec!["repair-head".encode().unwrap()],
        EntryKind::Data,
    );
    let tail_entry = Entry::new(
        tail_key,
        vec!["repair-tail".encode().unwrap()],
        EntryKind::Data,
    );
    let expected_head = head_entry.clone().try_into_storage_entry().unwrap();
    let expected_tail = tail_entry.clone().try_into_storage_entry().unwrap();
    node1
        .dht()
        .storage
        .put(&head_key.to_string(), &head_entry)
        .await
        .unwrap();
    node1
        .dht()
        .storage
        .put(&tail_key.to_string(), &tail_entry)
        .await
        .unwrap();
    for (key, owner) in [(head_key, head.did()), (tail_key, tail.did())] {
        assert_eq!(
            node1
                .dht()
                .next_hop_for_storage_sync(StorageSyncDestination::placement_key(key))
                .unwrap(),
            Some(owner)
        );
    }
    RepairFixture {
        placements: [
            RepairPlacement {
                owner: head.did(),
                key: head_key,
                expected: expected_head,
            },
            RepairPlacement {
                owner: tail.did(),
                key: tail_key,
                expected: expected_tail,
            },
        ],
    }
}

async fn exercise_contended_browser_storage(node1: &Swarm, node2: &Swarm) {
    node1
        .transport
        .start_outbound_frame_trace_for_test(node2.did());
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node2.did()),
        data: multi_frame_storage_sync_entries().unwrap(),
    };
    assert!(
        node1
            .transport
            .send_storage_sync(msg)
            .await
            .unwrap()
            .is_sent(),
        "browser storage contention send must not be deferred"
    );
    for round in 0..8 {
        node1
            .send_direct_message(
                Message::NotifyPredecessorSend(NotifyPredecessorSend { did: node1.did() }),
                node2.did(),
            )
            .await
            .unwrap_or_else(|error| panic!("control round {round} failed: {error}"));
        sleep(SOAK_POLL_INTERVAL).await;
        let trace = node1.transport.outbound_frame_trace_for_test(node2.did());
        if control_interleaves_transfer(&trace, MessageClass::Storage) {
            break;
        }
    }
    for _ in 0..SOAK_POLL_ATTEMPTS {
        let trace = node1.transport.outbound_frame_trace_for_test(node2.did());
        if control_interleaves_transfer(&trace, MessageClass::Storage) {
            break;
        }
        sleep(SOAK_POLL_INTERVAL).await;
    }
    let trace = node1
        .transport
        .take_outbound_frame_trace_for_test(node2.did());
    assert_control_interleaves_transfer(&trace, MessageClass::Storage);
}

async fn wait_for_repair_and_convergence(nodes: &[&Swarm; 3], fixture: &RepairFixture) {
    let [head, tail] = &fixture.placements;
    for _ in 0..REPAIR_POLL_ATTEMPTS {
        if repair_placement_matches(nodes, head).await
            && repair_placement_matches(nodes, tail).await
            && ring_topology_converged(nodes).unwrap()
        {
            return;
        }
        sleep(SOAK_POLL_INTERVAL).await;
    }
    panic!("real browser repair did not persist both remote placements and preserve convergence");
}

async fn repair_placement_matches(nodes: &[&Swarm; 3], placement: &RepairPlacement) -> bool {
    let owner = nodes
        .iter()
        .copied()
        .find(|node| node.did() == placement.owner);
    match owner {
        Some(node) => storage_matches(node, placement.key, &placement.expected).await,
        None => false,
    }
}

fn perturb_ring_predecessors(nodes: &[&Swarm; 3]) {
    for node in nodes {
        *node.dht().lock_predecessor().unwrap() = None;
    }
    assert!(!ring_topology_converged(nodes).unwrap());
}

fn start_browser_maintenance(
    nodes: &[Arc<Swarm>; 3],
    stop: &StopSource,
) -> Vec<futures::channel::oneshot::Receiver<()>> {
    let mut completions = Vec::new();
    for node in nodes {
        let stabilizer = Arc::new(node.stabilizer());
        let token = stop.token();
        let (completed, completion) = futures::channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            stabilizer
                .wait_with(BROWSER_MAINTENANCE_INTERVAL, token)
                .await;
            let _ = completed.send(());
        });
        completions.push(completion);
    }

    let pressure_swarm = nodes[0].clone();
    let pressure_token = stop.token();
    let (completed, completion) = futures::channel::oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        while !pressure_token.should_stop() {
            pressure_swarm.transport.request_storage_repair();
            sleep(SOAK_POLL_INTERVAL).await;
        }
        let _ = completed.send(());
    });
    completions.push(completion);
    completions
}

async fn stop_browser_maintenance(
    stop: StopSource,
    completions: Vec<futures::channel::oneshot::Receiver<()>>,
) {
    stop.request_stop();
    for completion in completions {
        completion
            .await
            .expect("browser maintenance task must stop cleanly");
    }
}

async fn wait_for_browser_maintenance_cadence(local: crate::dht::Did) {
    let deadline = web_time::Instant::now() + BROWSER_PHASE_TRACE_TIMEOUT;
    loop {
        let trace = maintenance_phase_trace_for_test(local);
        let stabilizations = trace
            .iter()
            .filter(|event| event.kind == MaintenancePhaseKind::Stabilize)
            .count();
        let repairs = trace
            .iter()
            .filter(|event| event.kind == MaintenancePhaseKind::Repair)
            .count();
        if stabilizations >= 2 && repairs >= 1 {
            assert_browser_maintenance_cadence(&trace);
            return;
        }
        assert!(
            web_time::Instant::now() < deadline,
            "browser maintenance did not expose two stabilization starts and one repair start"
        );
        sleep(SOAK_POLL_INTERVAL).await;
    }
}

fn assert_browser_maintenance_cadence(trace: &[MaintenancePhaseEvent]) {
    let stabilizations = trace
        .iter()
        .filter(|event| event.kind == MaintenancePhaseKind::Stabilize)
        .map(|event| event.started_at_ms)
        .collect::<Vec<_>>();
    let repairs = trace
        .iter()
        .filter(|event| event.kind == MaintenancePhaseKind::Repair)
        .map(|event| event.started_at_ms)
        .collect::<Vec<_>>();
    let interval_ms = u64::try_from(BROWSER_MAINTENANCE_INTERVAL.as_millis()).unwrap();
    assert!(stabilizations[0] >= interval_ms);
    assert!(stabilizations[0] <= 5_000);
    let stabilization_gap = stabilizations[1].saturating_sub(stabilizations[0]);
    assert!(stabilization_gap >= interval_ms);
    assert!(stabilization_gap <= 15_000);
    let first_repair_after_stabilization = repairs
        .iter()
        .copied()
        .find(|started_at_ms| *started_at_ms > stabilizations[0])
        .expect("repair phase must start after the first stabilization phase");
    let phase_offset = first_repair_after_stabilization.saturating_sub(stabilizations[0]);
    assert!(phase_offset >= SOAK_POLL_INTERVAL.as_millis() as u64);
    assert!(
        first_repair_after_stabilization < stabilizations[1],
        "repair phase must start before the next stabilization phase: stabilizations={stabilizations:?}, repairs={repairs:?}"
    );
}

struct DefaultCallback;
impl TransportCallback for DefaultCallback {}

async fn get_fake_permission() {
    let window = web_sys::window().unwrap();
    let nav = window.navigator();
    let media = nav.media_devices().unwrap();
    let cons = web_sys::MediaStreamConstraints::new();
    cons.set_audio(&JsValue::from(true));
    cons.set_video(&JsValue::from(false));
    cons.set_fake(true);
    let promise = media.get_user_media_with_constraints(&cons).unwrap();
    JsFuture::from(promise).await.unwrap();
}

async fn prepare_transport() -> Transport {
    let trans = Transport::new("stun://stun.l.google.com:19302", None, None);
    trans
        .new_connection("test", Box::new(DefaultCallback))
        .await
        .unwrap();
    trans
}

#[wasm_bindgen_test]
async fn test_ice_connection_establish() {
    get_fake_permission().await;
    let trans1 = prepare_transport().await;
    let conn1 = trans1.connection("test").unwrap();
    let trans2 = prepare_transport().await;
    let conn2 = trans2.connection("test").unwrap();

    assert_eq!(conn1.webrtc_connection_state(), WebrtcConnectionState::New);
    assert_eq!(conn2.webrtc_connection_state(), WebrtcConnectionState::New);

    let offer = conn1.webrtc_create_offer().await.unwrap();
    let answer = conn2.webrtc_answer_offer(offer).await.unwrap();
    conn1.webrtc_accept_answer(answer).await.unwrap();

    #[cfg(feature = "browser_chrome_test")]
    {
        conn2.webrtc_wait_for_data_channel_open().await.unwrap();
        assert_eq!(
            conn2.webrtc_connection_state(),
            WebrtcConnectionState::Connected
        );
    }
}

#[wasm_bindgen_test]
async fn test_message_handler() {
    get_fake_permission().await;

    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1, &node2).await;
}

#[wasm_bindgen_test]
async fn test_storage_repair_load_does_not_starve_three_node_stabilization() {
    let scenario = async {
        get_fake_permission().await;
        let nodes = prepare_repair_mesh().await;
        let [node1, node2, node3] = &nodes;
        let fixture = seed_remote_repair_entries(node1, node2, node3).await;
        let swarms = [node1.as_ref(), node2.as_ref(), node3.as_ref()];
        perturb_ring_predecessors(&swarms);
        reset_maintenance_phase_trace_for_test();
        let stop = StopSource::new();
        let completions = start_browser_maintenance(&nodes, &stop);
        exercise_contended_browser_storage(node1, node2).await;
        wait_for_repair_and_convergence(&swarms, &fixture).await;
        wait_for_browser_maintenance_cadence(node1.did()).await;
        stop_browser_maintenance(stop, completions).await;
    }
    .fuse();
    let deadline = sleep(BROWSER_REPAIR_SCENARIO_TIMEOUT).fuse();
    futures::pin_mut!(scenario, deadline);
    futures::select! {
        () = scenario => {}
        () = deadline => panic!("browser repair scenario exceeded its 60-second budget"),
    }
}
