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
use crate::swarm::transport::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
use crate::swarm::transport::OUTBOUND_DATA_TRANSFER_CAPACITY;
use crate::swarm::transport::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
use crate::tests::default::dummy_hooks::MaxMessageSizeGuard;
use crate::tests::default::dummy_hooks::PausedDeliveryGuard;
use crate::tests::default::dummy_hooks::PausedDispatchGuard;
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
async fn tracked_completion_releases_capacity_before_returning() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let payload = tracked_payload(&node1, peer, b"tracked-capacity-release")?;

    node1.swarm.transport.send_payload_tracked(payload).await?;

    assert_eq!(
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer),
        Some(0)
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_releases_batch_before_first_tracked_completion() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _pending_delivery = PendingDeliveryGuard::new();
    let first_payload = tracked_payload(&node1, peer, b"first-shutdown-transfer")?;
    let second_payload = tracked_payload(&node1, peer, b"second-shutdown-transfer")?;
    let first_swarm = node1.swarm.clone();
    let second_swarm = node1.swarm.clone();
    let mut first = tokio::spawn(async move {
        first_swarm
            .transport
            .send_payload_tracked(first_payload)
            .await
    });
    let mut second = tokio::spawn(async move {
        second_swarm
            .transport
            .send_payload_tracked(second_payload)
            .await
    });
    wait_until("tracked shutdown batch admission", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(2)
    })
    .await?;

    node1.swarm.transport.disconnect(peer).await?;
    let first_completed = timeout(Duration::from_secs(2), async {
        tokio::select! {
            result = &mut first => {
                let _ = result.map_err(|error| {
                    invalid_test_state(format!("first tracked task failed: {error}"))
                })?;
                Ok::<_, Error>(true)
            }
            result = &mut second => {
                let _ = result.map_err(|error| {
                    invalid_test_state(format!("second tracked task failed: {error}"))
                })?;
                Ok(false)
            }
        }
    })
    .await
    .map_err(|_| invalid_test_state("tracked shutdown batch did not complete"))??;

    assert!(node1
        .swarm
        .transport
        .outbound_admitted_transfer_count_for_test(peer)
        .is_none_or(|admitted| admitted == 0));
    let remaining = if first_completed { second } else { first };
    let _ = timeout(Duration::from_secs(2), remaining)
        .await
        .map_err(|_| invalid_test_state("remaining tracked shutdown task did not complete"))?
        .map_err(|error| invalid_test_state(format!("tracked shutdown task failed: {error}")))?;
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
async fn same_class_chunked_transfers_are_contiguous_on_the_wire() -> Result<()> {
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
async fn submitted_control_preempts_a_ready_bulk_tail() -> Result<()> {
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
            swarm
                .send_direct_message(Message::custom(body.as_bytes())?, peer)
                .await
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
            swarm
                .send_direct_message(
                    Message::PeerLivenessProbe(PeerLivenessProbe {
                        sent_at_ms: index as i64,
                    }),
                    peer,
                )
                .await
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
async fn transfer_capacity_bounds_real_waiting_and_queued_lifetimes() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _pending_delivery = PendingDeliveryGuard::new();
    let mut sends = spawn_data_capacity_transfers(&node1, peer);

    assert_data_capacity_is_bounded(&node1, peer).await?;
    spawn_reserved_control_transfers(&node1, peer, &mut sends);
    assert_control_capacity_is_bounded(&node1, peer).await?;
    disconnect_and_join_capacity_transfers(&node1, peer, sends).await
}

#[tokio::test]
async fn detached_delivery_timeout_releases_transfer_capacity() -> Result<()> {
    let measure = Arc::new(FailedSendMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;
    let (node1, node2) = connect_nodes(node1, node2).await?;
    let peer = node2.did();
    let _pending_delivery = PendingDeliveryGuard::new();

    node1
        .swarm
        .send_direct_message(Message::custom(b"bounded-detached-delivery")?, peer)
        .await?;
    wait_until("detached transfer admission", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(1)
    })
    .await?;
    timeout(Duration::from_secs(2), async {
        while node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            != Some(0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| invalid_test_state("detached delivery timeout retained capacity"))?;
    wait_for_msgs([&node1, &node2]).await;
    assert_eq!(
        measure.count(),
        0,
        "local delivery timeout must not degrade peer quality"
    );
    Ok(())
}

#[tokio::test]
async fn delivery_timeout_marks_generation_terminal_before_releasing_fifo_lane() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _paused_delivery = PausedDeliveryGuard::new();
    let _pending_close = PendingCloseGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_direct_message(Message::custom(b"first-stalled-delivery")?, peer)
        .await?;
    wait_until("first delivery gate", || {
        dummy_controlled::delivery_future_waiting()
    })
    .await?;

    let second_swarm = node1.swarm.clone();
    let second_send = tokio::spawn(async move {
        second_swarm
            .send_direct_message(Message::custom(b"second-fifo-transfer")?, peer)
            .await
    });
    wait_until("second FIFO transfer submission", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(2)
    })
    .await?;
    wait_until("timed-out generation send revocation", || {
        node1.swarm.transport.get_connection(peer).is_none()
    })
    .await?;
    assert!(node1.swarm.transport.is_admitted_connection(peer));

    dummy_controlled::release_delivery_future_gate();
    let second_result = timeout(Duration::from_secs(2), second_send)
        .await
        .map_err(|_| invalid_test_state("second FIFO submission did not finish"))?
        .map_err(|error| invalid_test_state(format!("second FIFO task failed: {error}")))?;
    assert!(second_result.is_err());
    assert_eq!(dummy_controlled::sent_count(), 1);
    timeout(
        Duration::from_secs(2),
        node1.swarm.stabilizer().clean_unavailable_connections(),
    )
    .await
    .map_err(|_| invalid_test_state("terminal generation cleanup stayed pending"))??;
    assert!(!node1.swarm.transport.is_admitted_connection(peer));
    Ok(())
}

#[tokio::test]
async fn outbound_capacity_is_reserved_before_readiness_wait() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _pending_open = PendingDataChannelOpenGuard::new();
    let swarm = node1.swarm.clone();
    let send = tokio::spawn(async move {
        swarm
            .send_direct_message(Message::custom(b"reserve-before-readiness")?, peer)
            .await
    });

    wait_until("capacity reservation before readiness", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(1)
    })
    .await?;
    send.abort();
    let _ = send.await;
    wait_until("cancelled readiness wait capacity release", || {
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
async fn pending_measurement_does_not_block_peer_scheduler() -> Result<()> {
    let started = Arc::new(AtomicBool::new(false));
    let measure: MeasureImpl = Arc::new(PendingMeasure {
        started: started.clone(),
    });
    let node1 = prepare_node_with_measure(SecretKey::random(), measure)?;
    let node2 = prepare_node(SecretKey::random()).await;
    let (node1, node2) = connect_nodes(node1, node2).await?;
    let peer = node2.did();

    node1
        .swarm
        .send_direct_message(Message::custom(b"first-measured-send")?, peer)
        .await?;
    wait_until("pending outbound measurement", || {
        started.load(Ordering::Acquire)
    })
    .await?;

    timeout(
        Duration::from_secs(1),
        node1
            .swarm
            .send_direct_message(Message::custom(b"scheduler-must-progress")?, peer),
    )
    .await
    .map_err(|_| invalid_test_state("measurement blocked the peer scheduler"))??;
    Ok(())
}

#[traced_test]
#[tokio::test]
async fn oversized_payload_log_omits_the_custom_message_body() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let marker = "private-oversized-custom-body";
    let oversized_bytes = TRANSPORT_MAX_SIZE + 8 * 1024 * 1024;
    let repeats = oversized_bytes / marker.len() + 1;
    let body = marker.repeat(repeats).into_bytes();
    let error = node1
        .swarm
        .send_direct_message(Message::CustomMessage(CustomMessage(body)), node2.did())
        .await
        .expect_err("oversized payload must be rejected before scheduling");

    assert!(matches!(error, Error::MessageTooLarge(size) if size > TRANSPORT_MAX_SIZE));
    assert!(logs_contain("message payload is too large"));
    assert!(!logs_contain(marker));
    Ok(())
}
