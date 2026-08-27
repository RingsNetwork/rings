//! End-to-end scheduler invariants over the controlled dummy transport.

use std::future::pending;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use rings_transport::connections::dummy_controlled;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio::time::Duration;
use tracing_test::traced_test;

use crate::chunk::Chunk;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::measure::PeerQuality;
use crate::message::CustomMessage;
use crate::message::FoundEntry;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::PeerLivenessProbe;
use crate::swarm::transport::outbound_submit_count_for_test;
use crate::swarm::transport::reset_outbound_submit_count_for_test;
use crate::swarm::transport::SendCompletionOutcome;
use crate::swarm::transport::OUTBOUND_COMMAND_DRAIN_BUDGET;
use crate::swarm::transport::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
use crate::swarm::transport::OUTBOUND_DATA_TRANSFER_CAPACITY;
use crate::swarm::transport::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
use crate::tests::default::dummy_hooks::MaxMessageSizeGuard;
use crate::tests::default::dummy_hooks::PausedDeliveryGuard;
use crate::tests::default::dummy_hooks::PausedDispatchGuard;
use crate::tests::default::dummy_hooks::PausedIrrevocableSendGuard;
use crate::tests::default::dummy_hooks::PendingAfterSentCountGuard;
use crate::tests::default::dummy_hooks::PendingCloseGuard;
use crate::tests::default::dummy_hooks::PendingDataChannelOpenGuard;
use crate::tests::default::dummy_hooks::PendingDeliveryGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::prepare_node_with_measure;
use crate::tests::default::wait_for_connection_state;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::default::TEST_WAIT_TIMEOUT;
use crate::tests::manually_establish_connection;
use crate::tests::outbound_capacity_released;

fn invalid_test_state(message: impl Into<String>) -> Error {
    Error::InvalidMessage(message.into())
}

async fn connected_nodes() -> Result<(Node, Node)> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    connect_nodes(node1, node2).await
}

fn tracked_payload(node: &Node, peer: Did, body: &[u8]) -> Result<MessagePayload> {
    MessagePayload::new_send(
        Message::custom(body)?,
        node.swarm.transport.session_sk(),
        peer,
        peer,
    )
}

#[tokio::test]
async fn test_tracked_completion_releases_capacity_before_returning() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let payload = tracked_payload(&node1, peer, b"tracked-capacity-release")?;

    node1.swarm.transport.send_payload_tracked(payload).await?;

    assert!(outbound_capacity_released(&node1.swarm.transport, peer));
    Ok(())
}

#[tokio::test]
async fn test_tracked_timeout_removes_queued_capacity_before_predecessor_finishes() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let paused_delivery = PausedDeliveryGuard::new();
    node1
        .swarm
        .send_message(Message::custom(b"tracked-lane-head")?, peer)
        .await?;
    let payload = tracked_payload(&node1, peer, b"tracked-queued-successor")?;

    let outcome = node1.swarm.transport.send_payload_tracked(payload).await?;

    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer),
        Some(1),
        "only the still-active predecessor may retain capacity"
    );
    drop(paused_delivery);
    wait_until("predecessor capacity release", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(0)
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_tracked_timeout_removes_target_behind_multiple_predecessors() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let paused_delivery = PausedDeliveryGuard::new();
    reset_outbound_submit_count_for_test();
    let mut predecessors = Vec::new();
    for index in 0..3 {
        let swarm = node1.swarm.clone();
        predecessors.push(tokio::spawn(async move {
            let body = format!("queued-predecessor-{index}");
            let payload = MessagePayload::new_send(
                Message::custom(body.as_bytes())?,
                swarm.transport.session_sk(),
                peer,
                peer,
            )?;
            swarm
                .transport
                .send_payload_detached_observing_scheduler_submit_for_test(payload, || {})
                .await
        }));
    }
    wait_until("three predecessor submissions", || {
        outbound_submit_count_for_test() == 3
    })
    .await?;
    let payload = tracked_payload(&node1, peer, b"cancel-behind-predecessors")?;

    let outcome = node1.swarm.transport.send_payload_tracked(payload).await?;

    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer),
        Some(3),
        "the cancelled target must release capacity without waiting for queued predecessors"
    );
    drop(paused_delivery);
    for predecessor in predecessors {
        timeout(Duration::from_secs(2), predecessor)
            .await
            .map_err(|_| invalid_test_state("timed out joining predecessor send"))?
            .map_err(|error| invalid_test_state(format!("predecessor task failed: {error}")))??;
    }
    wait_until("predecessor capacity cleanup", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(0)
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_irrevocable_send_timeout_releases_scheduler_capacity() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let payload = tracked_payload(&node1, peer, b"irrevocable-send-timeout")?;
    let paused_send = PausedIrrevocableSendGuard::new();
    dummy_controlled::reset_sent_count();

    let error = node1
        .swarm
        .transport
        .send_payload_tracked(payload)
        .await
        .expect_err("an irrevocable backend send must have a completion deadline");

    assert!(
        matches!(
            &error,
            Error::DataChannelSendCompletionTimeout { peer: timed_out, .. } if *timed_out == peer
        ),
        "unexpected cancellation result: {error:?}"
    );
    assert!(outbound_capacity_released(&node1.swarm.transport, peer));
    assert!(dummy_controlled::irrevocable_send_gate_waiting());
    drop(paused_send);
    wait_until("retired irrevocable dummy send completion", || {
        !dummy_controlled::irrevocable_send_gate_waiting()
    })
    .await?;
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "an irrevocable task that outlives its caller must not dispatch after connection retirement"
    );
    Ok(())
}

#[tokio::test]
async fn test_detached_deadline_cannot_succeed_after_irrevocable_chunk_admission() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _max_message_size = MaxMessageSizeGuard::new(8192);
    let paused_send = PausedIrrevocableSendGuard::new();
    dummy_controlled::reset_sent_count();
    let payload = tracked_payload(&node1, peer, &vec![7_u8; 50_000])?;
    let deadline = async {
        while !dummy_controlled::irrevocable_send_gate_waiting() {
            tokio::task::yield_now().await;
        }
    };

    let error = node1
        .swarm
        .transport
        .send_payload_detached_until_for_test(payload, Duration::from_millis(1), deadline)
        .await
        .expect_err("cancellation must win before first-frame success is published");

    assert!(
        matches!(
            &error,
            Error::DataChannelSendCompletionTimeout { peer: timed_out, .. } if *timed_out == peer
        ),
        "unexpected cancellation result: {error:?}"
    );
    assert!(outbound_capacity_released(&node1.swarm.transport, peer));
    drop(paused_send);
    wait_until("retired chunk send completion", || {
        !dummy_controlled::irrevocable_send_gate_waiting()
    })
    .await?;
    assert_eq!(
        dummy_controlled::sent_count(),
        0,
        "neither the first chunk nor any tail may dispatch after cancellation wins"
    );
    Ok(())
}

#[tokio::test]
async fn test_detached_first_frame_timeout_cancels_queued_transfer() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let paused_delivery = PausedDeliveryGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_message(Message::custom(b"lane-head")?, peer)
        .await?;
    let error = node1
        .swarm
        .send_message(Message::custom(b"queued-successor")?, peer)
        .await
        .expect_err("detached completion must not wait indefinitely behind a lane head");

    assert!(matches!(
        error,
        Error::OutboundFirstFrameAdmissionTimeout { peer: timed_out, .. } if timed_out == peer
    ));
    drop(paused_delivery);
    wait_until("timed-out detached transfer cancellation", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(0)
    })
    .await?;
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "the timed-out successor must stop before its first frame"
    );
    Ok(())
}

#[tokio::test]
async fn test_dropping_detached_caller_after_submit_cancels_queued_transfer() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let paused_delivery = PausedDeliveryGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_message(Message::custom(b"drop-lane-head")?, peer)
        .await?;
    let swarm = node1.swarm.clone();
    let successor = tokio::spawn(async move {
        swarm
            .send_message(Message::custom(b"drop-queued-successor")?, peer)
            .await
    });
    wait_until("detached successor scheduler admission", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(2)
    })
    .await?;

    successor.abort();
    wait_until("dropped detached successor cancellation", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(1)
    })
    .await?;
    drop(paused_delivery);
    wait_until("detached predecessor completion", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(0)
    })
    .await?;
    assert_eq!(
        dummy_controlled::sent_count(),
        1,
        "dropping the successor caller must stop it before its first frame"
    );
    Ok(())
}

#[tokio::test]
async fn test_cancelled_transfer_is_rejected_when_cancel_command_precedes_submit() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let paused_delivery = PausedDeliveryGuard::new();
    node1
        .swarm
        .send_message(Message::custom(b"pre-cancel-lane-head")?, peer)
        .await?;
    let payload = tracked_payload(&node1, peer, b"pre-cancelled-successor")?;

    let outcome = timeout(
        Duration::from_millis(100),
        node1
            .swarm
            .transport
            .send_payload_detached_cancel_before_submit_for_test(payload),
    )
    .await
    .map_err(|_| invalid_test_state("pre-cancelled transfer remained queued"))??;

    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer),
        Some(1),
        "only the active predecessor may retain capacity"
    );
    drop(paused_delivery);
    Ok(())
}

#[tokio::test]
async fn test_shutdown_releases_batch_before_first_tracked_completion() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    dummy_controlled::reset_sent_count();
    let _paused_dispatch = PausedDispatchGuard::new();
    node1.swarm.transport.pause_outbound_worker_for_test(peer);
    let payloads = (0..40)
        .map(|index| {
            tracked_payload(
                &node1,
                peer,
                format!("shutdown-transfer-{index}").as_bytes(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let tasks = payloads
        .into_iter()
        .map(|payload| {
            let swarm = node1.swarm.clone();
            tokio::spawn(async move {
                swarm
                    .transport
                    .send_payload_tracked_with_shutdown_deadline_for_test(payload)
                    .await
            })
        })
        .collect::<Vec<_>>();
    wait_until("tracked shutdown mailbox backlog", || {
        node1
            .swarm
            .transport
            .outbound_buffered_submissions_for_test(peer)
            > OUTBOUND_COMMAND_DRAIN_BUDGET
    })
    .await?;
    assert_eq!(
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer),
        Some(40)
    );

    node1.swarm.transport.resume_outbound_worker_for_test(peer);
    wait_until("active tracked shutdown transfer", || {
        node1
            .swarm
            .transport
            .outbound_worker_has_active_transfer_for_test(peer)
            && dummy_controlled::send_message_waiting_at_dispatch()
            && node1
                .swarm
                .transport
                .outbound_buffered_submissions_for_test(peer)
                > 0
            && dummy_controlled::sent_count() == 0
    })
    .await?;
    node1.swarm.transport.disconnect(peer).await?;
    for (index, task) in tasks.into_iter().enumerate() {
        let outcome = timeout(Duration::from_secs(2), task)
            .await
            .map_err(|_| invalid_test_state("tracked shutdown task did not complete"))?
            .map_err(|error| {
                invalid_test_state(format!("tracked shutdown task failed: {error}"))
            })??;
        assert_eq!(outcome, SendCompletionOutcome::Cancelled);
        if index == 0 {
            assert_eq!(
                node1
                    .swarm
                    .transport
                    .outbound_admitted_transfer_total_for_test(),
                0,
                "every shutdown permit must release before the first completion is published"
            );
        }
    }

    let rejected = node1
        .swarm
        .transport
        .send_payload_tracked_with_shutdown_deadline_for_test(tracked_payload(
            &node1,
            peer,
            b"post-shutdown-submission",
        )?)
        .await;
    assert!(matches!(rejected, Err(Error::SwarmMissDidInTable(missing)) if missing == peer));
    Ok(())
}

async fn connect_nodes(node1: Node, node2: Node) -> Result<(Node, Node)> {
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_connection_state(&node1, node2.did(), WebrtcConnectionState::Connected).await?;
    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    Ok((node1, node2))
}

struct PendingMeasure {
    started: Arc<AtomicBool>,
}

#[derive(Default)]
struct FailedSendMeasure {
    count: AtomicUsize,
}

impl FailedSendMeasure {
    fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

#[async_trait]
impl Measure for FailedSendMeasure {
    async fn incr(&self, _did: Did, counter: MeasureCounter) {
        if counter == MeasureCounter::FailedToSend {
            self.count.fetch_add(1, Ordering::AcqRel);
        }
    }

    async fn get_count(&self, _did: Did, counter: MeasureCounter) -> u64 {
        if counter == MeasureCounter::FailedToSend {
            self.count() as u64
        } else {
            0
        }
    }
}

#[async_trait]
impl BehaviourJudgement for FailedSendMeasure {
    async fn quality(&self, _did: Did) -> PeerQuality {
        PeerQuality::Unknown
    }

    async fn good(&self, _did: Did) -> bool {
        true
    }
}

#[async_trait]
impl Measure for PendingMeasure {
    async fn incr(&self, _did: Did, _counter: MeasureCounter) {
        self.started.store(true, Ordering::Release);
        pending::<()>().await;
    }

    async fn get_count(&self, _did: Did, _counter: MeasureCounter) -> u64 {
        0
    }
}

#[async_trait]
impl BehaviourJudgement for PendingMeasure {
    async fn quality(&self, _did: Did) -> PeerQuality {
        PeerQuality::Unknown
    }

    async fn good(&self, _did: Did) -> bool {
        true
    }
}

async fn wait_until(label: &str, condition: impl Fn() -> bool) -> Result<()> {
    timeout(TEST_WAIT_TIMEOUT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| invalid_test_state(format!("timed out waiting for {label}")))?;
    Ok(())
}

async fn await_send(task: JoinHandle<Result<uuid::Uuid>>, label: &str) -> Result<()> {
    let result = timeout(Duration::from_secs(2), task)
        .await
        .map_err(|_| invalid_test_state(format!("timed out waiting for {label}")))?
        .map_err(|error| invalid_test_state(format!("{label} task failed: {error}")))?;
    result?;
    Ok(())
}

fn message_chunk(message: Message) -> Option<Chunk> {
    match message {
        Message::Chunk(chunk) => Some(chunk),
        _ => None,
    }
}

async fn collect_two_chunk_transfers(node: &Node, first: Chunk) -> Result<Vec<Chunk>> {
    timeout(Duration::from_secs(5), async move {
        let first_id = first.meta.id;
        let first_total = first.chunk[1];
        let mut second = None;
        let mut trace = vec![first];

        loop {
            if let Some((_, second_total)) = second {
                if trace.len() == first_total.saturating_add(second_total) {
                    return Ok(trace);
                }
            }

            let payload = node
                .listen_once()
                .await
                .ok_or_else(|| invalid_test_state("message stream closed while tracing chunks"))?;
            let Some(chunk) = message_chunk(payload.transaction.data::<Message>()?) else {
                continue;
            };
            if chunk.meta.id != first_id {
                match second {
                    None => second = Some((chunk.meta.id, chunk.chunk[1])),
                    Some((second_id, _)) if second_id == chunk.meta.id => {}
                    Some(_) => {
                        return Err(invalid_test_state(
                            "a third chunk transfer appeared in the FIFO trace",
                        ));
                    }
                }
            }
            trace.push(chunk);
        }
    })
    .await
    .map_err(|_| invalid_test_state("timed out collecting complete chunk transfers"))?
}

fn assert_transfer_positions(chunks: &[&Chunk]) {
    let total = chunks.len();
    assert!(chunks
        .iter()
        .enumerate()
        .all(|(position, chunk)| chunk.chunk == [position, total]));
}

#[tokio::test]
async fn test_same_class_chunked_transfers_are_contiguous_on_the_wire() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _max_size = MaxMessageSizeGuard::new(8192);
    let _paused_delivery = PausedDeliveryGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_direct_message(Message::custom(&vec![0x11; 50_000])?, peer)
        .await?;
    wait_until("first chunk delivery gate", || {
        dummy_controlled::delivery_future_waiting()
    })
    .await?;
    let first_payload = node2
        .listen_once()
        .await
        .ok_or_else(|| invalid_test_state("expected the first chunk"))?;
    let first = message_chunk(first_payload.transaction.data::<Message>()?)
        .ok_or_else(|| invalid_test_state("the first wire message was not a chunk"))?;
    assert_eq!(first.chunk[0], 0);

    reset_outbound_submit_count_for_test();
    let second_swarm = node1.swarm.clone();
    let second_send = tokio::spawn(async move {
        second_swarm
            .send_direct_message(Message::custom(&vec![0x22; 40_000])?, peer)
            .await
    });
    wait_until("second application transfer submission", || {
        outbound_submit_count_for_test() >= 1
    })
    .await?;
    dummy_controlled::release_delivery_future_gate();

    let trace = collect_two_chunk_transfers(&node2, first).await?;
    await_send(second_send, "second application transfer").await?;
    let first_id = trace
        .first()
        .map(|chunk| chunk.meta.id)
        .ok_or_else(|| invalid_test_state("chunk trace was empty"))?;
    let second_id = trace
        .iter()
        .find(|chunk| chunk.meta.id != first_id)
        .map(|chunk| chunk.meta.id)
        .ok_or_else(|| invalid_test_state("second chunk transfer was not observed"))?;
    let first_chunks: Vec<_> = trace
        .iter()
        .filter(|chunk| chunk.meta.id == first_id)
        .collect();
    let second_chunks: Vec<_> = trace
        .iter()
        .filter(|chunk| chunk.meta.id == second_id)
        .collect();

    assert_transfer_positions(&first_chunks);
    assert_transfer_positions(&second_chunks);
    assert!(trace
        .iter()
        .take(first_chunks.len())
        .all(|chunk| chunk.meta.id == first_id));
    assert!(trace
        .iter()
        .skip(first_chunks.len())
        .all(|chunk| chunk.meta.id == second_id));
    Ok(())
}

#[tokio::test]
async fn test_submitted_control_preempts_a_ready_bulk_tail() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _max_size = MaxMessageSizeGuard::new(8192);
    let _paused_bulk_delivery = PausedDeliveryGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_direct_message(Message::custom(&vec![0x33; 50_000])?, peer)
        .await?;
    wait_until("bulk delivery gate", || {
        dummy_controlled::delivery_future_waiting()
    })
    .await?;
    let first = node2
        .listen_once()
        .await
        .ok_or_else(|| invalid_test_state("expected the first bulk chunk"))?;
    assert!(message_chunk(first.transaction.data::<Message>()?).is_some());

    let _paused_storage_dispatch = PausedDispatchGuard::new();
    let storage_swarm = node1.swarm.clone();
    let storage_send = tokio::spawn(async move {
        storage_swarm
            .send_direct_message(
                Message::FoundEntry(FoundEntry {
                    data: Vec::new(),
                    misses: Vec::new(),
                    resource: Did::from(91_u32),
                    redundancy: 1,
                }),
                peer,
            )
            .await
    });
    wait_until("storage dispatch gate", || {
        dummy_controlled::send_message_waiting_at_dispatch()
    })
    .await?;

    dummy_controlled::release_delivery_future_gate();
    reset_outbound_submit_count_for_test();
    let control_swarm = node1.swarm.clone();
    let control_send = tokio::spawn(async move {
        control_swarm
            .send_direct_message(
                Message::PeerLivenessProbe(PeerLivenessProbe { sent_at_ms: 41 }),
                peer,
            )
            .await
    });
    wait_until("control transfer submission", || {
        outbound_submit_count_for_test() >= 1
    })
    .await?;

    let _pending_after_control = PendingAfterSentCountGuard::new(3);
    dummy_controlled::release_send_message_gate();
    await_send(storage_send, "storage transfer").await?;
    await_send(control_send, "control transfer").await?;
    assert_eq!(dummy_controlled::sent_count(), 3);

    let observed = timeout(Duration::from_secs(2), async {
        let mut observed = Vec::new();
        loop {
            let payload = node2
                .listen_once()
                .await
                .ok_or_else(|| invalid_test_state("message stream closed before control frame"))?;
            match payload.transaction.data::<Message>()? {
                Message::Chunk(_) => observed.push("bulk"),
                Message::FoundEntry(_) => observed.push("storage"),
                Message::PeerLivenessProbe(_) => {
                    observed.push("control");
                    return Ok::<_, Error>(observed);
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| invalid_test_state("timed out observing control preemption"))??;
    assert_eq!(observed, vec!["storage", "control"]);
    Ok(())
}

fn spawn_data_capacity_transfers(node: &Node, peer: Did) -> Vec<JoinHandle<Result<uuid::Uuid>>> {
    let mut sends = Vec::with_capacity(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    for index in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        let swarm = node.swarm.clone();
        sends.push(tokio::spawn(async move {
            let body = format!("capacity-transfer-{index}");
            let payload = MessagePayload::new_send(
                Message::custom(body.as_bytes())?,
                swarm.transport.session_sk(),
                peer,
                peer,
            )?;
            let tx_id = payload.transaction.tx_id;
            swarm
                .transport
                .send_payload_detached_observing_scheduler_submit_for_test(payload, || {})
                .await
                .map(|_| tx_id)
        }));
    }
    sends
}

async fn assert_data_capacity_is_bounded(node: &Node, peer: Did) -> Result<()> {
    wait_until("non-control transfer capacity", || {
        node.swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(OUTBOUND_DATA_TRANSFER_CAPACITY)
    })
    .await?;

    let data_error = node
        .swarm
        .send_direct_message(Message::custom(b"over-data-capacity")?, peer)
        .await
        .expect_err("non-control work must not consume the control reserve");
    assert!(matches!(
        data_error,
        Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            capacity: OUTBOUND_DATA_TRANSFER_CAPACITY,
        } if error_peer == peer
    ));
    assert!(data_error.is_deferrable_data_plane_send());
    assert!(!data_error.records_peer_send_failure());
    Ok(())
}

fn spawn_reserved_control_transfers(
    node: &Node,
    peer: Did,
    sends: &mut Vec<JoinHandle<Result<uuid::Uuid>>>,
) {
    for index in 0..OUTBOUND_CONTROL_RESERVED_TRANSFERS {
        let swarm = node.swarm.clone();
        sends.push(tokio::spawn(async move {
            let payload = MessagePayload::new_send(
                Message::PeerLivenessProbe(PeerLivenessProbe {
                    sent_at_ms: index as i64,
                }),
                swarm.transport.session_sk(),
                peer,
                peer,
            )?;
            let tx_id = payload.transaction.tx_id;
            swarm
                .transport
                .send_payload_detached_observing_scheduler_submit_for_test(payload, || {})
                .await
                .map(|_| tx_id)
        }));
    }
}

async fn assert_control_capacity_is_bounded(node: &Node, peer: Did) -> Result<()> {
    wait_until("reserved control transfer capacity", || {
        node.swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(OUTBOUND_DATA_TRANSFER_CAPACITY + OUTBOUND_CONTROL_RESERVED_TRANSFERS)
    })
    .await?;
    let control_error = node
        .swarm
        .send_direct_message(
            Message::PeerLivenessProbe(PeerLivenessProbe { sent_at_ms: 999 }),
            peer,
        )
        .await
        .expect_err("control traffic must preserve the other data-class reserves");
    assert!(matches!(
        control_error,
        Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            ..
        } if error_peer == peer
    ));
    Ok(())
}

async fn disconnect_and_join_capacity_transfers(
    node: &Node,
    peer: Did,
    sends: Vec<JoinHandle<Result<uuid::Uuid>>>,
) -> Result<()> {
    node.swarm.transport.disconnect(peer).await?;
    for send in sends {
        let result = timeout(Duration::from_secs(2), send)
            .await
            .map_err(|_| invalid_test_state("capacity transfer did not stop on disconnect"))?
            .map_err(|error| invalid_test_state(format!("capacity task failed: {error}")))?;
        if let Err(error) = result {
            assert!(matches!(
                error,
                Error::OutboundTransferCapacityExceeded { .. }
                    | Error::OutboundTransferMemoryCapacityExceeded { .. }
                    | Error::OutboundTransferAdmissionTimeout { .. }
                    | Error::OutboundFirstFrameAdmissionTimeout { .. }
                    | Error::ConnectionAttemptSuperseded { .. }
                    | Error::ChannelSendMessageFailed
            ));
        }
    }
    wait_until("retired scheduler capacity release", || {
        node.swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            .is_none()
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_transfer_capacity_bounds_real_waiting_and_queued_lifetimes() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _pending_delivery = PendingDeliveryGuard::new();
    let mut sends = spawn_data_capacity_transfers(&node1, peer);

    assert_data_capacity_is_bounded(&node1, peer).await?;
    spawn_reserved_control_transfers(&node1, peer, &mut sends);
    assert_control_capacity_is_bounded(&node1, peer).await?;
    disconnect_and_join_capacity_transfers(&node1, peer, sends).await
}

mod test_boundaries;
mod test_delivery;
