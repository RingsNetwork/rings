use std::future::Future;
use std::pin::Pin;

use super::*;

const BARRIER_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const STARTED_REASSEMBLY_FRAMES: usize = 28;

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
    let TrackedStorageSyncOutcome::Delivered(storage_tx) = outcome else {
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
                .send_direct_message(
                    Message::custom(b"sync-storm-lower-class")
                        .expect("mixed-load application message must encode"),
                    peer,
                )
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
                    .send_direct_message(
                        Message::PeerLivenessProbe(PeerLivenessProbe {
                            sent_at_ms: i64::try_from(TEST_EPOCH_MS)
                                .expect("epoch must fit i64")
                                .saturating_add(ordinal as i64),
                        }),
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
        while matches!(
            futures::poll!(sends.next()),
            std::task::Poll::Ready(Some(()))
        ) {}
        if transport.outbound_submitted_transfers_for_test(peer) >= minimum_submissions {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("mixed control/storage transfers did not reach the live worker mailbox");
}

async fn wait_for_mixed_frames(runtime: &SimulationRuntimeGuard) {
    for _ in 0..64 {
        let pending = runtime
            .pending_deliveries()
            .expect("mixed-load frames must classify");
        let has_control = pending
            .iter()
            .any(|delivery| delivery.class == ScheduledDeliveryClass::Control);
        let has_application = pending
            .iter()
            .any(|delivery| delivery.class == ScheduledDeliveryClass::Application);
        if has_control && has_application {
            return;
        }
        settle_one_poll().await;
    }
    panic!("mixed control/application frames did not reach the dummy queue");
}

async fn drain_control_burst(runtime: &SimulationRuntimeGuard) {
    for _ in 0..=OUTBOUND_CONTROL_BURST {
        wait_for_optional_class(runtime, ScheduledDeliveryClass::Control).await;
        let has_control = runtime
            .pending_deliveries()
            .expect("control-pressure frames must classify")
            .iter()
            .any(|delivery| delivery.class == ScheduledDeliveryClass::Control);
        if has_control {
            deliver_one_class(runtime, ScheduledDeliveryClass::Control).await;
            settle_one_poll().await;
        }
    }
}

async fn wait_for_optional_class(runtime: &SimulationRuntimeGuard, class: ScheduledDeliveryClass) {
    for _ in 0..64 {
        if runtime
            .pending_deliveries()
            .expect("pressure frames must classify")
            .iter()
            .any(|delivery| delivery.class == class)
        {
            return;
        }
        settle_one_poll().await;
    }
}

pub(super) async fn exercise_barrier_control_exemption(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    kind: ScenarioTopology,
    driver: &mut TraceDriver,
) {
    let (sender, receiver) = physical_edges(nodes, kind)[0];
    let peer = nodes[receiver].did();
    let (mut reassembly_deliveries, reassembly) =
        start_barrier_backlog(runtime, nodes, sender, peer, driver).await;

    let mut control_send = nodes[sender]
        .swarm
        .send_direct_message(
            Message::PeerLivenessProbe(PeerLivenessProbe {
                sent_at_ms: i64::try_from(TEST_EPOCH_MS).expect("epoch must fit i64"),
            }),
            peer,
        )
        .boxed_local();
    let mut control_send_complete = false;
    wait_for_class_while_sending(
        runtime,
        ScheduledDeliveryClass::Control,
        control_send.as_mut(),
        &mut control_send_complete,
        "barrier control send must complete",
    )
    .await;
    let control = delivery_for_class(runtime, ScheduledDeliveryClass::Control);
    driver.observe_pending(runtime, std::slice::from_ref(&control));
    let deadline = control
        .deadline_virtual_ms
        .expect("control pressure must carry an explicit deadline");
    let mut control_delivery = runtime.deliver(&control).boxed_local();
    let blocked_control = !crate::simulation::protection_profile().barrier_control_exemption();
    let control_complete =
        wait_for_control_barrier_verdict(runtime, control_delivery.as_mut(), blocked_control).await;
    driver.observe_dispatch(&control);
    driver.observe_barrier(&control, blocked_control);
    persist_runtime_artifact("barrier-control-verdict", runtime)
        .expect("barrier verdict artifact must be writable");
    if blocked_control {
        observe_barrier_deadline_miss(runtime, deadline, control_delivery.as_mut()).await;
    }
    drain_started_reassembly(runtime, &mut reassembly_deliveries).await;
    for delivery in &reassembly {
        driver.observe_delivery(runtime, delivery);
    }
    if !control_complete {
        assert!(control_delivery
            .as_mut()
            .await
            .expect("blocked control delivery must resume after reassembly service"));
    }
    driver.observe_delivery(runtime, &control);
    driver.observe_production_handlers(runtime);
    finish_send(control_send.as_mut(), control_send_complete).await;
    runtime
        .disable_reassembly_service()
        .expect("deterministic reassembly service must disable");
    drain_bootstrap(runtime, nodes).await;
    dummy_controlled::set_max_message_size(CHUNK_MESSAGE_SIZE);
    drop(control_delivery);
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
    dummy_controlled::set_max_message_size(crate::consts::TRANSPORT_MTU);
    let large_application = Message::custom(&vec![0x5a; BARRIER_PAYLOAD_BYTES])
        .expect("chunked barrier payload must encode");
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
    let reassembly = runtime
        .pending_deliveries()
        .expect("barrier reassembly frames must classify")
        .into_iter()
        .filter(|delivery| delivery.class == ScheduledDeliveryClass::Reassembly)
        .take(STARTED_REASSEMBLY_FRAMES)
        .collect::<Vec<_>>();
    assert_eq!(
        reassembly.len(),
        STARTED_REASSEMBLY_FRAMES,
        "barrier pressure must fill the real inbound reassembly lane"
    );
    driver.observe_pending(runtime, &reassembly);
    let mut reassembly_deliveries = start_controlled_deliveries(runtime, reassembly.clone());
    assert!(matches!(
        futures::poll!(reassembly_deliveries.next()),
        std::task::Poll::Pending
    ));
    settle_one_poll().await;
    for delivery in &reassembly {
        driver.observe_dispatch(delivery);
    }
    (reassembly_deliveries, reassembly)
}

pub(super) async fn wait_for_control_barrier_verdict<F>(
    runtime: &SimulationRuntimeGuard,
    mut control_delivery: Pin<&mut F>,
    blocked_control: bool,
) -> bool
where
    F: Future<Output = Result<bool, crate::simulation::SimulationRuntimeError>> + ?Sized,
{
    for _ in 0..128 {
        match futures::poll!(control_delivery.as_mut()) {
            std::task::Poll::Ready(result) => {
                assert!(
                    !blocked_control,
                    "legacy barrier unexpectedly admitted control"
                );
                assert!(result.expect("enabled control delivery must remain stable"));
                return true;
            }
            std::task::Poll::Pending => {}
        }
        let observed = runtime
            .protection_observations()
            .expect("barrier observation must remain visible")
            .barrier_control_blocked();
        if observed {
            assert!(
                blocked_control,
                "enabled control reached a forbidden barrier block"
            );
            return false;
        }
        tokio::task::yield_now().await;
    }
    panic!("control event produced neither delivery nor a production barrier verdict");
}

pub(super) type ControlledDelivery<'a> =
    futures::future::LocalBoxFuture<'a, Result<bool, crate::simulation::SimulationRuntimeError>>;

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

async fn observe_barrier_deadline_miss<F>(
    runtime: &SimulationRuntimeGuard,
    deadline: u64,
    mut control_delivery: Pin<&mut F>,
) where
    F: Future<Output = Result<bool, crate::simulation::SimulationRuntimeError>> + ?Sized,
{
    let elapsed = u64::try_from(runtime.elapsed_ms().expect("elapsed time must fit"))
        .expect("pressure time must fit u64");
    let delta_ms = deadline.saturating_sub(elapsed).saturating_add(1);
    runtime
        .advance(std::time::Duration::from_millis(delta_ms))
        .await
        .expect("barrier deadline must advance deterministically");
    let observed = u64::try_from(runtime.elapsed_ms().expect("elapsed time must fit"))
        .expect("pressure time must fit u64");
    assert!(observed > deadline);
    assert!(matches!(
        futures::poll!(control_delivery.as_mut()),
        std::task::Poll::Pending
    ));
    crate::simulation::record_barrier_control_deadline_miss(observed, deadline);
    persist_runtime_artifact("barrier-control-deadline-miss", runtime)
        .expect("barrier deadline artifact must be writable");
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
        if runtime
            .pending_deliveries()
            .expect("barrier workload must classify")
            .iter()
            .any(|delivery| delivery.class == class)
        {
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
        while matches!(
            futures::poll!(sends.next()),
            std::task::Poll::Ready(Some(()))
        ) {}
        if sends.is_empty() && dummy_controlled::pending() == 0 && !network_busy(nodes) {
            return;
        }
        settle_one_poll().await;
    }
    panic!("scheduler-pressure workload did not quiesce within the derived harness bound");
}
