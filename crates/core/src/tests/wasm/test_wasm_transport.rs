use std::sync::Arc;
use std::time::Duration;

use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use super::prepare_node;
use super::prepare_repair_node;
use super::with_hang_guard;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::maintenance_phase_trace_for_test;
use crate::dht::reset_maintenance_phase_trace_for_test;
use crate::dht::topology;
use crate::dht::MaintenancePhaseEvent;
use crate::dht::MaintenancePhaseKind;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::lifecycle::StopSource;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::NotifyPredecessorSend;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::Transport;
use crate::swarm::Swarm;
use crate::tests::activity::probe_on_activity;
use crate::tests::activity::swarms_quiescent;
use crate::tests::assert_control_interleaves_transfer;
use crate::tests::control_interleaves_transfer;
use crate::tests::data_frame_count;
use crate::tests::data_transfer_progressed;
use crate::tests::live_entry;
use crate::tests::manually_establish_connection;
use crate::tests::midpoint_storage_key;
use crate::tests::multi_frame_storage_sync_entries;
use crate::tests::replace_observed_fingers;
use crate::tests::ring_topology_converged;
use crate::tests::tail_storage_key;
use crate::utils::sleep;

/// Pace of the repair-pressure producer. It is the load under test, not a wait; also the
/// minimum offset the maintenance cadence assertion expects between phases.
const REPAIR_PRESSURE_INTERVAL: Duration = Duration::from_millis(25);
const BROWSER_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(500);
/// Hang guard of the whole browser repair soak; every soak wait inside it is activity-woken.
const BROWSER_REPAIR_SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
/// Hang guard of one awaited soak state; the scenario hang guard bounds the whole run.
const SOAK_STATE_HANG_GUARD: Duration = BROWSER_REPAIR_SCENARIO_TIMEOUT;
/// Hang guard of one real browser handshake. The fixtures gather host candidates only, so a
/// same-page handshake completes far below it; see `with_hang_guard` for the budget arithmetic.
const BROWSER_HANDSHAKE_HANG_GUARD: Duration = Duration::from_secs(15);

/// Await, on activity, the state in which every node routes to every other.
async fn wait_for_full_mesh(nodes: &[&Swarm]) {
    probe_on_activity(
        "three-node browser mesh routable",
        SOAK_STATE_HANG_GUARD,
        || {
            let connected = nodes.iter().all(|node| {
                nodes
                    .iter()
                    .all(|peer| node.did() == peer.did() || node.peer_dids().contains(&peer.did()))
            });
            async move { Ok(connected.then_some(())) }
        },
    )
    .await
    .unwrap();
}

/// Await, on activity, the converged three-node ring.
async fn wait_for_ring_convergence(nodes: &[&Swarm]) {
    probe_on_activity(
        "three-node browser ring converged",
        SOAK_STATE_HANG_GUARD,
        || {
            let converged = ring_topology_converged(nodes);
            async move { converged.map(|converged| converged.then_some(())) }
        },
    )
    .await
    .unwrap();
}

/// Await, on activity, the quiescent mesh; see [`swarms_quiescent`].
async fn wait_for_quiescence(nodes: &[&Swarm]) {
    probe_on_activity("browser mesh quiescent", SOAK_STATE_HANG_GUARD, || {
        let quiescent = swarms_quiescent(nodes.iter().copied());
        async move { Ok(quiescent.then_some(())) }
    })
    .await
    .unwrap();
}

/// Run one stabilization round on every node and await its traffic to settle.
///
/// A round is not re-issued while its reports are in flight: `begin_stabilization` supersedes
/// an unanswered round, so re-issuing early would make every report stale. The next round
/// therefore waits for the quiescent mesh, an observed state, not for a duration.
async fn run_stabilization_round(nodes: &[&Swarm; 3]) {
    let [first_node, second_node, third_node] = *nodes;
    let first = first_node.stabilizer();
    let second = second_node.stabilizer();
    let third = third_node.stabilizer();
    let (first, second, third) =
        futures::join!(first.stabilize(), second.stabilize(), third.stabilize(),);
    first.unwrap();
    second.unwrap();
    third.unwrap();
    wait_for_quiescence(nodes).await;
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
    wait_for_quiescence(&swarms).await;
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
    nodes
}

async fn seed_remote_repair_entries(node1: &Swarm, node2: &Swarm, node3: &Swarm) -> RepairFixture {
    let mut routed_peers = [node2, node3];
    routed_peers.sort_by_key(|node| topology::dist(node1.did(), node.did()));
    let [head, tail] = routed_peers;
    replace_observed_fingers(node1, &[(0, head.did()), (3, tail.did())]).unwrap();
    let head_key = midpoint_storage_key(node1.did(), head.did(), tail.did());
    let tail_key = tail_storage_key(node1.did(), tail.did());
    // Stamped live: a value written to storage without a retention bound is retired on its
    // next read and would never be republished.
    let head_entry = live_entry(
        head_key,
        vec![b"repair-head".as_slice().encode().unwrap()],
        EntryKind::Data,
    );
    let tail_entry = live_entry(
        tail_key,
        vec![b"repair-tail".as_slice().encode().unwrap()],
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
        // Each control follows a storage frame admitted after the previous control, so the
        // controls interleave with the transfer rather than precede it. The wait is on the
        // transfer's progress; the control's own activity does not satisfy it.
        let trace = node1.transport.outbound_frame_trace_for_test(node2.did());
        if control_interleaves_transfer(&trace, MessageCategory::Storage) {
            break;
        }
        let admitted = data_frame_count(&trace, MessageCategory::Storage);
        node1
            .send_direct_message(
                Message::NotifyPredecessorSend(NotifyPredecessorSend { did: node1.did() }),
                node2.did(),
            )
            .await
            .unwrap_or_else(|error| panic!("control round {round} failed: {error}"));
        probe_on_activity(
            &format!("the storage transfer progresses after control {round}"),
            SOAK_STATE_HANG_GUARD,
            || {
                let progressed = data_transfer_progressed(
                    &node1.transport.outbound_frame_trace_for_test(node2.did()),
                    MessageCategory::Storage,
                    admitted,
                    node1.transport.outbound_admitted_transfer_total_for_test(),
                );
                async move { Ok(progressed.then_some(())) }
            },
        )
        .await
        .unwrap();
    }
    probe_on_activity(
        "control interleaves the storage transfer",
        SOAK_STATE_HANG_GUARD,
        || {
            let trace = node1.transport.outbound_frame_trace_for_test(node2.did());
            let interleaved = control_interleaves_transfer(&trace, MessageCategory::Storage);
            async move { Ok(interleaved.then_some(())) }
        },
    )
    .await
    .unwrap();
    let trace = node1
        .transport
        .take_outbound_frame_trace_for_test(node2.did());
    assert_control_interleaves_transfer(&trace, MessageCategory::Storage);
}

/// Await, on activity, both repaired placements and the converged ring.
async fn wait_for_repair_and_convergence(nodes: &[&Swarm; 3], fixture: &RepairFixture) {
    let [head, tail] = &fixture.placements;
    probe_on_activity(
        "real browser repair persisted both remote placements and preserved convergence",
        SOAK_STATE_HANG_GUARD,
        || async {
            let reached = repair_placement_matches(nodes, head).await
                && repair_placement_matches(nodes, tail).await
                && ring_topology_converged(nodes)?;
            Ok(reached.then_some(()))
        },
    )
    .await
    .unwrap();
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
            sleep(REPAIR_PRESSURE_INTERVAL).await;
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

/// Await, on activity, two stabilization starts and one repair start in the maintenance trace;
/// every phase start sends messages, so it is observed as activity.
async fn wait_for_browser_maintenance_cadence(local: crate::dht::Did) {
    let trace = probe_on_activity(
        "browser maintenance exposes two stabilization starts and one repair start",
        SOAK_STATE_HANG_GUARD,
        || {
            let trace = maintenance_phase_trace_for_test(local);
            let stabilizations = trace
                .iter()
                .filter(|event| event.kind == MaintenancePhaseKind::Stabilize)
                .count();
            let repairs = trace
                .iter()
                .filter(|event| event.kind == MaintenancePhaseKind::Repair)
                .count();
            let reached = stabilizations >= 2 && repairs >= 1;
            async move { Ok(reached.then_some(trace)) }
        },
    )
    .await
    .unwrap();
    assert_browser_maintenance_cadence(&trace);
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
    assert!(phase_offset >= REPAIR_PRESSURE_INTERVAL.as_millis() as u64);
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
    let trans = Transport::new(super::TEST_ICE_SERVERS, None, None);
    trans
        .new_connection("test", Box::new(DefaultCallback))
        .await
        .unwrap();
    trans
}

/// Real browser ICE between two raw transports.
///
/// This test exercises the real transport by design, so it runs under a per-test hang guard:
/// a stalled handshake fails here by name instead of timing out the whole binary.
#[wasm_bindgen_test]
async fn test_ice_connection_establish() {
    with_hang_guard(
        "test_ice_connection_establish",
        BROWSER_HANDSHAKE_HANG_GUARD,
        async {
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
        },
    )
    .await
}

/// Real browser handshake between two swarms, under a per-test hang guard (see
/// [`test_ice_connection_establish`]).
#[wasm_bindgen_test]
async fn test_message_handler_manual_handshake_only() {
    with_hang_guard(
        "test_message_handler_manual_handshake_only",
        BROWSER_HANDSHAKE_HANG_GUARD,
        async {
            get_fake_permission().await;

            let key1 = SecretKey::random();
            let key2 = SecretKey::random();

            let node1 = prepare_node(key1).await;
            let node2 = prepare_node(key2).await;

            manually_establish_connection(&node1, &node2).await;
        },
    )
    .await
}

/// Browser storage repair under load does not starve three-node stabilization.
///
/// Every wait of the soak is a state probe woken by activity (#882), so
/// `BROWSER_REPAIR_SCENARIO_TIMEOUT` is a hang guard: it bounds a failure and never paces a
/// wait. CI still runs this test as its own invocation (`qaci.yml`), so a hang in it cannot
/// consume the 120 s runner budget of the rest of the binary.
#[wasm_bindgen_test]
async fn test_storage_repair_load_does_not_starve_three_node_stabilization() {
    with_hang_guard(
        "browser repair scenario",
        BROWSER_REPAIR_SCENARIO_TIMEOUT,
        async {
            get_fake_permission().await;
            let nodes = prepare_repair_mesh().await;
            let [node1, node2, node3] = &nodes;
            let fixture = seed_remote_repair_entries(node1, node2, node3).await;
            let swarms = [node1.as_ref(), node2.as_ref(), node3.as_ref()];
            perturb_ring_predecessors(&swarms);
            reset_maintenance_phase_trace_for_test();
            let stop = StopSource::new();
            let completions =
                start_browser_maintenance(&[node1.clone(), node2.clone(), node3.clone()], &stop);
            exercise_contended_browser_storage(node1, node2).await;
            wait_for_repair_and_convergence(&swarms, &fixture).await;
            wait_for_browser_maintenance_cadence(node1.did()).await;
            stop_browser_maintenance(stop, completions).await;
        },
    )
    .await
}
