use std::future::Future;
use std::pin::Pin;

use super::*;
use crate::swarm::application::ApplicationData;

const BARRIER_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
/// The frames one lane may have in flight: its credit window.
const LANE_CREDIT_WINDOW_FRAMES: usize = rings_transport::core::credit::LANE_CREDIT_WINDOW as usize;
pub(super) async fn exercise_per_entry_yield(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
) {
    dummy_controlled::set_max_message_size(crate::consts::TRANSPORT_MAX_SIZE);
    let (receiver, keys, storage_tx) = submit_yield_pressure(nodes, kind).await;
    let progress_notify = runtime
        .arm_storage_progress_probe()
        .expect("storage progress probe must arm");
    let progress_observer = crate::simulation::spawn_storage_progress_observer(progress_notify)
        .expect("storage progress observer requires the simulation Tokio runtime");
    let storage = runtime
        .pending_deliveries()
        .expect("yield-pressure frame must classify")
        .into_iter()
        .find(|delivery| delivery.transaction_id == Some(storage_tx))
        .expect("yield-pressure transfer must retain its production transaction identity");
    assert_eq!(storage.class, ScheduledDeliveryClass::Storage);
    assert!(runtime
        .deliver(&storage)
        .await
        .expect("storage progress delivery must remain stable"));
    drain_untraced(runtime, nodes).await;
    verify_yield_progress(runtime, nodes, receiver, &keys, progress_observer).await;
    runtime
        .disarm_storage_progress_probe()
        .expect("storage progress probe must disarm");
    persist_runtime_artifact("per-entry-yield-pressure", runtime)
        .expect("per-entry yield pressure artifact must be writable");
    remove_yield_pressure_entries(nodes, receiver, keys).await;
    dummy_controlled::set_max_message_size(CHUNK_MESSAGE_SIZE);
}

async fn submit_yield_pressure(
    nodes: &[Node],
    kind: ScenarioTopology,
) -> (usize, Vec<crate::dht::Did>, uuid::Uuid) {
    let (sender, receiver) = physical_edges(nodes, kind)[0];
    let data = (0..2)
        .map(|index| entry_owned_by(&nodes[receiver], &format!("yield-pressure-{index}")))
        .collect::<Vec<_>>();
    let keys = data.iter().map(|placed| placed.key).collect::<Vec<_>>();
    let outcome = nodes[sender]
        .swarm
        .transport
        .send_storage_sync_tracked(SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::AdditiveRepair,
            destination: StorageSyncDestination::PhysicalOwner(nodes[receiver].did()),
            data,
        })
        .await
        .expect("yield-pressure sync must enter the real scheduler");
    let StorageSyncOutcome::Sent(storage_tx) = outcome else {
        panic!("yield-pressure sync must be delivered remotely: {outcome:?}");
    };
    (receiver, keys, storage_tx)
}

async fn verify_yield_progress(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    receiver: usize,
    keys: &[crate::dht::Did],
    progress_observer: tokio::task::JoinHandle<()>,
) {
    for _ in 0..128 {
        if progress_observer.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let mut persisted_pressure_entries = 0usize;
    for key in keys {
        if nodes[receiver]
            .dht()
            .storage
            .get(&key.to_string())
            .await
            .expect("yield-pressure storage must remain readable")
            .is_some()
        {
            persisted_pressure_entries = persisted_pressure_entries.saturating_add(1);
        }
    }
    assert!(
        progress_observer.is_finished(),
        "storage persistence must wake the independent progress observer; epoch={} persisted={persisted_pressure_entries}",
        runtime
            .storage_progress_epoch()
            .expect("storage progress epoch must remain visible")
    );
    progress_observer
        .await
        .expect("independent storage progress observer must complete");
    assert!(
        runtime
            .storage_progress_epoch()
            .expect("storage progress epoch must remain visible")
            > 0,
        "the independently scheduled observer must consume the persistence wakeup"
    );
    if crate::simulation::protection_profile().per_entry_yield() {
        assert!(
            runtime
                .protection_observations()
                .expect("storage progress witness must remain visible")
                .storage_progress_between_entries(),
            "observer progress must be sampled after the first persistence and before the second"
        );
    }
}

async fn remove_yield_pressure_entries(
    nodes: &[Node],
    receiver: usize,
    keys: Vec<crate::dht::Did>,
) {
    for key in keys {
        nodes[receiver]
            .dht()
            .storage
            .remove(&key.to_string())
            .await
            .expect("yield-pressure fixture must be removed after observation");
    }
}

pub(super) async fn exercise_bounded_control_burst(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
) {
    let (sender, receiver) = physical_edges(nodes, kind)[0];
    let peer = nodes[receiver].did();
    let transport = &nodes[sender].swarm.transport;
    transport.start_outbound_frame_trace_for_test(peer);
    transport.pause_outbound_worker_for_test(peer);
    let mut sends = mixed_pressure_sends(nodes, sender, peer);

    wait_for_worker_submissions(transport, peer, &mut sends).await;
    transport.resume_outbound_worker_for_test(peer);
    wait_for_mixed_frames(runtime).await;
    persist_runtime_artifact("bounded-control-burst-pressure", runtime)
        .expect("bounded control burst artifact must be writable");
    transport.pause_outbound_worker_for_test(peer);
    deliver_one_class(runtime, ScheduledDeliveryClass::Control).await;
    deliver_one_class(runtime, ScheduledDeliveryClass::Application).await;
    settle_one_poll().await;
    transport.resume_outbound_worker_for_test(peer);
    drain_control_burst(runtime).await;

    drain_pressure_futures(runtime, nodes, &mut sends).await;
    let _ = transport.take_outbound_frame_trace_for_test(peer);
}

fn mixed_pressure_sends<'a>(
    nodes: &'a [Node],
    sender: usize,
    peer: crate::dht::Did,
) -> FuturesUnordered<futures::future::LocalBoxFuture<'a, ()>> {
    let sends = FuturesUnordered::new();
    sends.push(
        async move {
            nodes[sender]
                .swarm
                .send_direct_message(ApplicationData::custom(b"sync-storm-lower-class"), peer)
                .await
                .expect("mixed-load application send must complete");
        }
        .boxed_local(),
    );
    for ordinal in 0..OUTBOUND_CONTROL_BURST + 3 {
        sends.push(
            async move {
                nodes[sender]
                    .swarm
                    .send_direct_core_message_for_test(
                        Message::ProbeRequest(test_probe_request(
                            u8::try_from(ordinal).unwrap_or(u8::MAX),
                        )),
                        peer,
                    )
                    .await
                    .expect("mixed-load control send must complete");
            }
            .boxed_local(),
        );
    }
    sends
}

async fn wait_for_worker_submissions(
    transport: &crate::swarm::transport::SwarmTransport,
    peer: crate::dht::Did,
    sends: &mut FuturesUnordered<futures::future::LocalBoxFuture<'_, ()>>,
) {
    let minimum_submissions = OUTBOUND_CONTROL_BURST + 2;
    for _ in 0..64 {
        poll_completed_sends(sends).await;
        if transport.outbound_submitted_transfers_for_test(peer) >= minimum_submissions {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("mixed control/storage transfers did not reach the live worker mailbox");
}

async fn wait_for_mixed_frames(runtime: &SimulationRuntimeGuard) {
    for _ in 0..64 {
        if has_pending_class(runtime, ScheduledDeliveryClass::Control)
            && has_pending_class(runtime, ScheduledDeliveryClass::Application)
        {
            return;
        }
        settle_one_poll().await;
    }
    panic!("mixed control/application frames did not reach the dummy queue");
}

async fn drain_control_burst(runtime: &SimulationRuntimeGuard) {
    for _ in 0..=OUTBOUND_CONTROL_BURST {
        wait_for_optional_class(runtime, ScheduledDeliveryClass::Control).await;
        if has_pending_class(runtime, ScheduledDeliveryClass::Control) {
            deliver_one_class(runtime, ScheduledDeliveryClass::Control).await;
            settle_one_poll().await;
        }
    }
}

async fn wait_for_optional_class(runtime: &SimulationRuntimeGuard, class: ScheduledDeliveryClass) {
    for _ in 0..64 {
        if has_pending_class(runtime, class) {
            return;
        }
        settle_one_poll().await;
    }
}

/// Hold an application reassembly backlog toward a peer, and probe that peer through the
/// production stabilizer behind it: the reassembly barrier orders each category alone, so the
/// probe is not held back by another category's reassembly, is delivered at once and answered,
/// and the healthy peer is kept with no repair following (load-bounds paper, Prop. "Order per
/// category", #927; before #924 a barrier that held control starved the probe into a false
/// disconnect whose repair refed the storm).
pub(super) async fn exercise_reassembly_barrier_per_category(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
    driver: &mut TraceDriver,
) {
    let (sender, receiver) = physical_edges(nodes, kind)[0];
    let peer = nodes[receiver].did();
    let (mut reassembly_deliveries, reassembly) =
        start_barrier_backlog(runtime, nodes, sender, peer, driver).await;

    let repairs_before = runtime
        .repair_entries_observed()
        .expect("repair observation must remain available");
    let control = issue_stabilizer_probe(runtime, nodes, sender, receiver, driver).await;
    let mut control_delivery = runtime.deliver(&control).boxed_local();
    await_control_past_the_backlog(runtime, control_delivery.as_mut()).await;
    driver.observe_dispatch(&control);
    driver.observe_barrier(&control, false);
    persist_runtime_artifact("barrier-control-verdict", runtime)
        .expect("barrier verdict artifact must be writable");
    drain_started_reassembly(runtime, &mut reassembly_deliveries).await;
    for delivery in &reassembly {
        driver.observe_delivery(runtime, delivery);
    }
    driver.observe_delivery(runtime, &control);
    driver.observe_production_handlers(runtime);
    drop(control_delivery);
    runtime
        .disable_reassembly_service()
        .expect("deterministic reassembly service must disable");
    deliver_probe_answer(runtime, driver, &control).await;
    drain_bootstrap(runtime, nodes).await;
    assert_peer_kept_by_its_answer(runtime, nodes, sender, receiver, repairs_before).await;
    dummy_controlled::set_max_message_size(CHUNK_MESSAGE_SIZE);
}

/// Make `sender`'s probe of `receiver` due, and no other of its probes, then run the production
/// probe pass and return the one probe it queued; the probe's clock starts at its issue.
async fn issue_stabilizer_probe(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    sender: usize,
    receiver: usize,
    driver: &mut TraceDriver,
) -> ScheduledDelivery {
    let transport = &nodes[sender].swarm.transport;
    for (index, node) in nodes.iter().enumerate() {
        if index != sender && index != receiver && transport.has_active_connection(node.did()) {
            transport
                .restart_peer_liveness_period_for_test(node.did(), 0)
                .expect("non-target peer liveness must be restartable");
        }
    }
    let peer = nodes[receiver].did();
    transport
        .force_peer_liveness_probe_due(peer)
        .expect("target peer liveness must be forceable");
    nodes[sender]
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await
        .expect("production stabilizer must send its real liveness probe");
    let issued_at_ms =
        i64::try_from(crate::utils::get_epoch_ms()).expect("simulated epoch must fit liveness");
    assert_eq!(
        transport
            .peer_liveness_outstanding_since_for_test(peer)
            .expect("active peer liveness state must remain readable"),
        Some(issued_at_ms),
        "the liveness clock must originate at the real stabilizer probe"
    );
    let new_deliveries = new_deliveries_with_controls(runtime, 1).await;
    driver.observe_pending(runtime, &new_deliveries);
    let probes = new_deliveries
        .into_iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Control)
        .collect::<Vec<_>>();
    let [probe] = probes.as_slice() else {
        panic!("the pass must queue exactly the target's probe: {probes:?}");
    };
    assert!(probe.transaction_id.is_some());
    assert!(probe.deadline_virtual_ms.is_some());
    probe.clone()
}

/// Deliver the peer's answer to `probe`, under the probe's transaction.
async fn deliver_probe_answer(
    runtime: &SimulationRuntimeGuard,
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
}

/// The probe was answered: none is left to expire into a false disconnect, `sender` keeps
/// `receiver` through a liveness pass, and no repair entry followed.
async fn assert_peer_kept_by_its_answer(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    sender: usize,
    receiver: usize,
    repairs_before: u64,
) {
    let peer = nodes[receiver].did();
    let transport = &nodes[sender].swarm.transport;
    assert_eq!(
        transport
            .peer_liveness_outstanding_since_for_test(peer)
            .expect("active peer liveness state must remain readable"),
        None,
        "the probe must be answered once the barrier released it"
    );
    nodes[sender]
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await
        .expect("liveness evidence must be processed");
    assert!(transport.get_connection(peer).is_some());
    assert_eq!(
        runtime
            .repair_entries_observed()
            .expect("repair observation must remain available"),
        repairs_before,
        "no repair follows an answered probe"
    );
}

async fn start_barrier_backlog<'a>(
    runtime: &'a SimulationRuntimeGuard,
    nodes: &[Node],
    sender: usize,
    peer: crate::dht::Did,
    driver: &mut TraceDriver,
) -> (
    FuturesUnordered<ControlledDelivery<'a>>,
    Vec<ScheduledDelivery>,
) {
    dummy_controlled::set_max_message_size(super::CHUNKED_MAX_MESSAGE_SIZE);
    let large_application = ApplicationData::custom(&vec![0x5a; BARRIER_PAYLOAD_BYTES]);
    let mut application_send = nodes[sender]
        .swarm
        .send_direct_message(large_application, peer)
        .boxed_local();
    let mut application_send_complete = false;
    wait_for_class_while_sending(
        runtime,
        ScheduledDeliveryClass::Reassembly,
        application_send.as_mut(),
        &mut application_send_complete,
        "chunked application send must complete",
    )
    .await;
    finish_send(application_send.as_mut(), application_send_complete).await;
    runtime
        .enable_reassembly_service()
        .expect("deterministic reassembly service must enable");
    let reassembly = in_flight_reassembly(runtime);
    driver.observe_pending(runtime, &reassembly);
    let reassembly_deliveries = start_lane_handover(runtime, reassembly.clone()).await;
    settle_one_poll().await;
    for delivery in &reassembly {
        driver.observe_dispatch(delivery);
    }
    (reassembly_deliveries, reassembly)
}

/// Poll `control_delivery` while the reassembly backlog is held, until it completes: a control
/// frame behind another category's reassembly is dispatched at once, and the production barrier
/// never records holding control back.
async fn await_control_past_the_backlog<F>(
    runtime: &SimulationRuntimeGuard,
    mut control_delivery: Pin<&mut F>,
) where
    F: Future<Output = Result<bool, crate::simulation::SimulationRuntimeError>> + ?Sized,
{
    for _ in 0..128 {
        if let std::task::Poll::Ready(result) = futures::poll!(control_delivery.as_mut()) {
            assert!(result.expect("control delivery must remain stable"));
            return;
        }
        let waited = runtime
            .protection_observations()
            .expect("barrier observation must remain visible")
            .control_waited_for_reassembly();
        assert!(
            !waited,
            "control waited behind another category's reassembly"
        );
        tokio::task::yield_now().await;
    }
    panic!("control behind another category's reassembly was not delivered");
}

pub(super) type ControlledDelivery<'a> =
    futures::future::LocalBoxFuture<'a, Result<bool, crate::simulation::SimulationRuntimeError>>;

/// Every chunk frame in flight, the backlog a reassembly-pressure scenario starts.
///
/// Law: flow control lets a sender have at most its lane's free credit in flight,
/// `limit − committed`, which the credit law bounds by one window and the batched advertising
/// keeps above zero; the rest of a longer transfer waits at its sender.
pub(super) fn in_flight_reassembly(runtime: &SimulationRuntimeGuard) -> Vec<ScheduledDelivery> {
    let reassembly = runtime
        .pending_deliveries()
        .expect("reassembly frames in flight must classify")
        .into_iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Reassembly)
        .collect::<Vec<_>>();
    assert!(
        !reassembly.is_empty(),
        "reassembly pressure must put chunk frames in flight"
    );
    assert!(
        reassembly.len() <= LANE_CREDIT_WINDOW_FRAMES,
        "flow control must bound the frames in flight on one lane by its credit window"
    );
    reassembly
}

/// Start the deliveries of one lane's frames, under an enabled reassembly service, and return
/// the deliveries still in progress.
///
/// Law: arrival never waits on the protocol. Every frame but the one being handed over joins
/// the lane's FIFO at once and its delivery completes; the hand-over waits for the reassembly
/// service, so exactly the draining delivery remains.
pub(super) async fn start_lane_handover<'a>(
    runtime: &'a SimulationRuntimeGuard,
    deliveries: Vec<ScheduledDelivery>,
) -> FuturesUnordered<ControlledDelivery<'a>> {
    let mut started = start_controlled_deliveries(runtime, deliveries);
    while let std::task::Poll::Ready(Some(arrived)) = futures::poll!(started.next()) {
        assert!(arrived.expect("reassembly arrival must remain stable"));
    }
    assert_eq!(
        started.len(),
        1,
        "exactly the hand-over waits for the reassembly service; every other arrival completes"
    );
    started
}

pub(super) fn start_controlled_deliveries<'a>(
    runtime: &'a SimulationRuntimeGuard,
    deliveries: Vec<ScheduledDelivery>,
) -> FuturesUnordered<ControlledDelivery<'a>> {
    deliveries
        .into_iter()
        .map(|delivery| {
            async move { runtime.deliver(&delivery).await }.boxed_local() as ControlledDelivery<'a>
        })
        .collect()
}

pub(super) async fn drain_started_reassembly(
    runtime: &SimulationRuntimeGuard,
    deliveries: &mut FuturesUnordered<ControlledDelivery<'_>>,
) {
    while !deliveries.is_empty() {
        while let std::task::Poll::Ready(Some(result)) = futures::poll!(deliveries.next()) {
            assert!(result.expect("reassembly delivery must remain stable"));
        }
        if deliveries.is_empty() {
            break;
        }
        runtime
            .advance(std::time::Duration::from_millis(2_000))
            .await
            .expect("reassembly service must advance deterministically");
        settle_one_poll().await;
    }
}

async fn wait_for_class_while_sending<F, T, E>(
    runtime: &SimulationRuntimeGuard,
    class: ScheduledDeliveryClass,
    mut send: Pin<&mut F>,
    send_complete: &mut bool,
    failure: &str,
) where
    F: Future<Output = Result<T, E>> + ?Sized,
    E: std::fmt::Debug,
{
    for _ in 0..128 {
        if !*send_complete {
            if let std::task::Poll::Ready(result) = futures::poll!(send.as_mut()) {
                result.expect(failure);
                *send_complete = true;
            }
        }
        if has_pending_class(runtime, class) {
            return;
        }
        settle_one_poll().await;
    }
    panic!("{class:?} did not reach the dummy queue during barrier pressure");
}

async fn finish_send<F, T, E>(mut send: Pin<&mut F>, complete: bool)
where
    F: Future<Output = Result<T, E>> + ?Sized,
    E: std::fmt::Debug,
{
    if !complete {
        send.as_mut()
            .await
            .expect("pressure send must complete after the barrier releases");
    }
}

/// Whether a frame of `class` waits in the controlled delivery queue.
fn has_pending_class(runtime: &SimulationRuntimeGuard, class: ScheduledDeliveryClass) -> bool {
    runtime
        .pending_deliveries()
        .expect("pressure frames must classify")
        .iter()
        .any(|delivery| delivery.class == class)
}

/// Poll `sends` until none is ready, dropping every completed send.
async fn poll_completed_sends(
    sends: &mut FuturesUnordered<futures::future::LocalBoxFuture<'_, ()>>,
) {
    while matches!(
        futures::poll!(sends.next()),
        std::task::Poll::Ready(Some(()))
    ) {}
}

async fn deliver_one_class(runtime: &SimulationRuntimeGuard, class: ScheduledDeliveryClass) {
    let delivery = delivery_for_class(runtime, class);
    assert!(runtime
        .deliver(&delivery)
        .await
        .expect("pressure delivery must remain stable"));
}

fn delivery_for_class(
    runtime: &SimulationRuntimeGuard,
    class: ScheduledDeliveryClass,
) -> ScheduledDelivery {
    runtime
        .pending_deliveries()
        .expect("pressure frame must classify")
        .into_iter()
        .find(|delivery| delivery.class == class)
        .unwrap_or_else(|| panic!("missing {class:?} frame during scheduler pressure"))
}

async fn drain_pressure_futures(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    sends: &mut FuturesUnordered<futures::future::LocalBoxFuture<'_, ()>>,
) {
    for _ in 0..MAX_DRAIN_STEPS {
        if let Some(delivery) = runtime
            .select_delivery(DeliveryStrategy::Fifo)
            .expect("pressure cleanup frame must classify")
        {
            assert!(runtime
                .deliver(&delivery)
                .await
                .expect("pressure cleanup delivery must remain stable"));
        }
        poll_completed_sends(sends).await;
        if sends.is_empty() && dummy_controlled::pending() == 0 && !network_busy(nodes) {
            return;
        }
        settle_one_poll().await;
    }
    panic!("scheduler-pressure workload did not quiesce within the derived harness bound");
}

/// The deliveries queued since the last inspection, once `controls` control frames are among
/// them. A probe pass returns when its probes are queued in core, before the outbound workers
/// dispatch them, so this waits for each frame woken by the queue's enqueue signal, enabled
/// before the queue is read; the timeout is a hang guard, not a pace.
pub(super) async fn new_deliveries_with_controls(
    runtime: &SimulationRuntimeGuard,
    controls: usize,
) -> Vec<ScheduledDelivery> {
    let collect = async {
        let mut seen = Vec::new();
        loop {
            let signal = dummy_controlled::enqueue_signal();
            let mut enqueued = std::pin::pin!(signal.notified());
            enqueued.as_mut().enable();
            seen.extend(
                runtime
                    .new_pending_deliveries()
                    .expect("queued deliveries must classify"),
            );
            let queued = seen
                .iter()
                .filter(|delivery| delivery.class == ScheduledDeliveryClass::Control)
                .count();
            if queued >= controls {
                return seen;
            }
            enqueued.await;
        }
    };
    tokio::time::timeout(Duration::from_secs(60), collect)
        .await
        .expect("every queued probe must reach the delivery queue")
}
