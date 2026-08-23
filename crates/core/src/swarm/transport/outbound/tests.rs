use super::capacity::OUTBOUND_DATA_RESERVED_TRANSFERS;
use super::*;
use crate::ecc::SecretKey;
use crate::message::e2e::E2eHandshakeRequest;
use crate::message::Message;

type TestTransfer = (TransferClass, &'static str);

fn pop_and_finish(queues: &mut TransferQueues<TestTransfer>) -> Option<&'static str> {
    let transfer = queues.pop()?;
    let class = transfer.class();
    let (_, item) = transfer.into_item();
    queues.record_frame_admitted(class);
    queues.finish_current(class);
    Some(item)
}

fn pop_order(mut queues: TransferQueues<TestTransfer>, n: usize) -> Vec<&'static str> {
    let mut order = Vec::new();
    for _ in 0..n {
        let Some(item) = pop_and_finish(&mut queues) else {
            break;
        };
        order.push(item);
    }
    order
}

fn push(queues: &mut TransferQueues<TestTransfer>, class: TransferClass, item: &'static str) {
    queues.push(class, (class, item));
}

fn assert_wire_classification(
    message: &Message,
    expected_kind: &'static str,
    expected_class: TransferClass,
) {
    let typed = OutboundMessageMeta::from_message(message);
    let wire = rings_codec::serialize(message).expect("message must serialize");
    let decoded = OutboundMessageMeta::from_wire(&wire).expect("wire metadata must decode");

    assert_eq!(typed.kind().as_str(), expected_kind);
    assert_eq!(typed.class(), expected_class);
    assert_eq!(decoded.kind(), typed.kind());
    assert_eq!(decoded.class(), typed.class());
}

fn admit_single_frame_transfer(
    queues: &mut TransferQueues<TestTransfer>,
    delivery_id: u64,
) -> &'static str {
    let active = queues.pop().expect("a runnable transfer must exist");
    let class = active.class();
    let label = active.item().1;
    queues.record_frame_admitted(class);
    queues.wait_for_delivery(delivery_id, active);
    let delivered = queues
        .take_waiting(class, delivery_id)
        .expect("the matching delivery must own the lane head");
    queues.make_runnable(delivered);
    let completion_probe = queues.pop().expect("completion probe must be runnable");
    assert_eq!(completion_probe.item().1, label);
    queues.finish_current(class);
    label
}

#[test]
fn dht_control_preempts_queued_bulk_work() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Application, "app-1");
    push(&mut queues, TransferClass::Application, "app-2");
    push(&mut queues, TransferClass::DhtControl, "dht");

    assert_eq!(pop_and_finish(&mut queues), Some("dht"));
}

#[test]
fn continuous_dht_control_yields_to_lower_classes() {
    let mut queues = TransferQueues::default();
    for item in ["dht-1", "dht-2", "dht-3", "dht-4", "dht-5"] {
        push(&mut queues, TransferClass::DhtControl, item);
    }
    push(&mut queues, TransferClass::Application, "app");

    assert_eq!(pop_order(queues, 6), vec![
        "dht-1", "dht-2", "dht-3", "dht-4", "app", "dht-5"
    ]);
}

#[test]
fn completion_probes_do_not_consume_control_frame_burst() {
    let mut queues = TransferQueues::default();
    for item in ["dht-1", "dht-2", "dht-3", "dht-4", "dht-5"] {
        push(&mut queues, TransferClass::DhtControl, item);
    }
    push(&mut queues, TransferClass::Application, "app");

    let mut admitted = Vec::new();
    for delivery_id in 0..3 {
        admitted.push(admit_single_frame_transfer(&mut queues, delivery_id));
    }

    let active = queues.pop().expect("fourth control transfer must exist");
    assert_eq!(active.item().1, "dht-4");
    let class = active.class();
    queues.record_frame_admitted(class);
    admitted.push(active.item().1);
    queues.wait_for_delivery(3, active);
    let delivered = queues
        .take_waiting(TransferClass::DhtControl, 3)
        .expect("fourth control delivery must resume its lane");
    queues.make_runnable(delivered);

    admitted.push(pop_and_finish(&mut queues).expect("application frame must receive its slot"));
    let completed_control = queues
        .pop()
        .expect("fourth control completion probe must remain runnable");
    assert_eq!(completed_control.item().1, "dht-4");
    queues.finish_current(completed_control.class());
    admitted.push(admit_single_frame_transfer(&mut queues, 4));

    assert_eq!(admitted, vec![
        "dht-1", "dht-2", "dht-3", "dht-4", "app", "dht-5"
    ]);
}

#[test]
fn lower_classes_progress_round_robin() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Storage, "storage-1");
    push(&mut queues, TransferClass::Storage, "storage-2");
    push(&mut queues, TransferClass::E2e, "e2e");
    push(&mut queues, TransferClass::Application, "app");

    assert_eq!(pop_order(queues, 4), vec![
        "storage-1",
        "e2e",
        "app",
        "storage-2"
    ]);
}

#[test]
fn terminal_attempt_does_not_advance_the_lower_class_cursor() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Storage, "stale-storage");
    push(&mut queues, TransferClass::E2e, "e2e");
    push(&mut queues, TransferClass::Application, "application");

    let stale = queues
        .pop()
        .expect("storage must start at the initial cursor");
    assert_eq!(stale.item().1, "stale-storage");
    queues.discard(stale);
    push(&mut queues, TransferClass::Storage, "more-stale-storage");

    let next = queues
        .pop()
        .expect("a failed attempt must not count as a frame admission");
    assert_eq!(next.item().1, "more-stale-storage");
}

#[test]
fn cancelled_control_attempts_do_not_consume_the_control_burst() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Application, "application");
    for index in 0..OUTBOUND_CONTROL_BURST {
        push(&mut queues, TransferClass::DhtControl, "cancelled-control");
        let control = queues
            .pop()
            .expect("control must run while its burst remains");
        assert_eq!(
            control.item().1,
            "cancelled-control",
            "control attempt {index}"
        );
        queues.discard(control);
    }
    push(&mut queues, TransferClass::DhtControl, "fifth-control");

    let next = queues
        .pop()
        .expect("control must retain priority until a frame is admitted");
    assert_eq!(next.item().1, "fifth-control");
}

#[test]
fn waiting_lane_preserves_same_class_fifo_and_allows_control_preemption() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Application, "app-1");
    push(&mut queues, TransferClass::Application, "app-2");

    let active = queues.pop().expect("first application transfer must exist");
    assert_eq!(active.item(), &(TransferClass::Application, "app-1"));
    queues.record_frame_admitted(TransferClass::Application);
    queues.wait_for_delivery(7, active);
    push(&mut queues, TransferClass::DhtControl, "dht");
    let resumed = queues
        .take_waiting(TransferClass::Application, 7)
        .expect("matching application delivery must resume the lane");
    queues.make_runnable(resumed);

    assert_eq!(pop_and_finish(&mut queues), Some("dht"));
    assert_eq!(pop_and_finish(&mut queues), Some("app-1"));
    assert_eq!(pop_and_finish(&mut queues), Some("app-2"));
}

#[test]
fn draining_a_waiting_lane_returns_its_active_and_queued_transfers() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Application, "app-1");
    push(&mut queues, TransferClass::Application, "app-2");

    let active = queues
        .pop()
        .expect("active transfer must exist before drain");
    queues.record_frame_admitted(TransferClass::Application);
    queues.wait_for_delivery(11, active);

    let mut drained: Vec<_> = queues
        .drain_transfers()
        .into_iter()
        .map(|(_, label)| label)
        .collect();
    drained.sort_unstable();
    assert_eq!(drained, vec!["app-1", "app-2"]);
}

#[test]
fn transfer_capacity_is_strict_across_permit_lifetimes() {
    let global = Arc::new(GlobalTransferCapacity::new());
    let capacity = Arc::new(TransferCapacity::new(global));
    let peer = Did::from(7_u32);
    let mut permits = Vec::with_capacity(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    for _ in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        permits.push(
            capacity
                .try_acquire(peer, TransferClass::Application, 1)
                .expect("non-control capacity must admit its declared bound"),
        );
    }

    assert!(matches!(
        capacity.try_acquire(peer, TransferClass::Application, 1),
        Err(Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            capacity: OUTBOUND_DATA_TRANSFER_CAPACITY,
        }) if error_peer == peer
    ));
    for _ in 0..OUTBOUND_CONTROL_RESERVED_TRANSFERS {
        permits.push(
            capacity
                .try_acquire(peer, TransferClass::DhtControl, 1)
                .expect("reserved control capacity must remain available"),
        );
    }
    for class in [TransferClass::Storage, TransferClass::E2e] {
        for _ in 0..OUTBOUND_DATA_RESERVED_TRANSFERS {
            permits.push(
                capacity
                    .try_acquire(peer, class, 1)
                    .expect("every data class retains its reserved capacity"),
            );
        }
    }
    assert!(matches!(
        capacity.try_acquire(peer, TransferClass::DhtControl, 1),
        Err(Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            capacity,
        }) if error_peer == peer
            && capacity == super::capacity::transfer_limit(TransferClass::DhtControl)
    ));
    let backpressure = Error::OutboundTransferCapacityExceeded {
        peer,
        capacity: OUTBOUND_TRANSFER_QUEUE_CAPACITY,
    };
    assert!(backpressure.is_deferrable_data_plane_send());
    assert!(!backpressure.records_peer_send_failure());
    permits.pop();
    assert!(capacity.try_acquire(peer, TransferClass::E2e, 1).is_ok());
}

#[test]
fn transfer_memory_capacity_is_weighted_and_released() {
    let global = Arc::new(GlobalTransferCapacity::new());
    let capacity = Arc::new(TransferCapacity::new(global));
    let peer = Did::from(9_u32);
    let data_limit = super::capacity::peer_byte_limit(TransferClass::Application);
    let permit = capacity
        .try_acquire(peer, TransferClass::Application, data_limit)
        .expect("one transfer may consume the non-control byte budget");

    assert_eq!(capacity.admitted_bytes(), data_limit);
    assert!(matches!(
        capacity.try_acquire(peer, TransferClass::Application, 1),
        Err(Error::OutboundTransferMemoryCapacityExceeded {
            peer: error_peer,
            requested_bytes: 1,
            capacity_bytes,
        }) if error_peer == peer && capacity_bytes == data_limit
    ));
    drop(permit);
    assert_eq!(capacity.admitted_bytes(), 0);
}

#[test]
fn shutdown_closes_channel_without_worker_owned_sender() {
    let (sender, mut receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    let stop = StopSource::new();
    let handle = OutboundPeerHandle {
        peer: Did::from(8_u32),
        state: Arc::new(OutboundPeerState {
            sender: Mutex::new(sender),
            #[cfg(not(target_family = "wasm"))]
            capacity: Arc::new(TransferCapacity::new(Arc::new(
                GlobalTransferCapacity::new(),
            ))),
            stop: stop.clone(),
        }),
    };

    handle.shutdown();

    assert!(stop.is_stop_requested());
    assert!(matches!(receiver.next().now_or_never(), Some(None)));
}

#[test]
fn final_handle_drop_requests_stop_before_channel_close() {
    let (sender, mut receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    let stop = StopSource::new();
    let handle = OutboundPeerHandle {
        peer: Did::from(10_u32),
        state: Arc::new(OutboundPeerState {
            sender: Mutex::new(sender),
            #[cfg(not(target_family = "wasm"))]
            capacity: Arc::new(TransferCapacity::new(Arc::new(
                GlobalTransferCapacity::new(),
            ))),
            stop: stop.clone(),
        }),
    };

    drop(handle);

    assert!(stop.is_stop_requested());
    assert!(matches!(receiver.next().now_or_never(), Some(None)));
}

#[test]
fn message_classification_is_local_and_control_first() {
    let dht = Message::PeerLivenessProbe(crate::message::PeerLivenessProbe { sent_at_ms: 1 });
    let storage = Message::SyncEntriesWithSuccessor(crate::message::SyncEntriesWithSuccessor {
        purpose: crate::dht::StorageSyncPurpose::AdditiveRepair,
        destination: crate::dht::StorageSyncDestination::PhysicalOwner(Did::from(1_u32)),
        data: Vec::new(),
    });
    let requester_public_key = SecretKey::random().pubkey();
    let e2e = Message::E2eHandshakeRequest(E2eHandshakeRequest::new(requester_public_key));
    let query = Message::QueryForTopoInfoSend(crate::message::QueryForTopoInfoSend::new_for_sync(
        Did::from(2_u32),
    ));
    let chunk = Message::Chunk(crate::chunk::Chunk {
        chunk: [0, 1],
        data: Bytes::new(),
        meta: crate::chunk::ChunkMeta::default(),
    });
    let app = Message::custom(b"hello").expect("custom message must build");

    assert_wire_classification(&dht, "PeerLivenessProbe", TransferClass::DhtControl);
    assert_wire_classification(&storage, "SyncEntriesWithSuccessor", TransferClass::Storage);
    assert_wire_classification(&app, "CustomMessage", TransferClass::Application);
    assert_wire_classification(&e2e, "E2eHandshakeRequest", TransferClass::E2e);
    assert_wire_classification(&query, "QueryForTopoInfoSend", TransferClass::DhtControl);
    assert_wire_classification(&chunk, "Chunk", TransferClass::Application);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn peer_capacity_survives_scheduler_generation_replacement() {
    let schedulers = OutboundSchedulers::new(None);
    let peer = Did::from(11_u32);
    let first = schedulers
        .handle(peer)
        .expect("first scheduler generation must start");
    let mut permits = Vec::with_capacity(OUTBOUND_DATA_TRANSFER_CAPACITY);
    for _ in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        permits.push(
            first
                .reserve(TransferClass::Application, 1)
                .expect("first generation must fill its declared capacity"),
        );
    }

    schedulers.shutdown(peer);
    let replacement = schedulers
        .handle(peer)
        .expect("replacement scheduler generation must start");

    assert!(matches!(
        replacement.reserve(TransferClass::Application, 1),
        Err(Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            capacity: OUTBOUND_DATA_TRANSFER_CAPACITY,
        }) if error_peer == peer
    ));
    permits.pop();
    assert!(replacement.reserve(TransferClass::Application, 1).is_ok());
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn dead_peer_capacity_keys_are_pruned() {
    let schedulers = OutboundSchedulers::new(None);
    let first = schedulers
        .reserve(Did::from(41_u32), TransferClass::Application, 1)
        .await
        .expect("first peer must reserve capacity");
    assert_eq!(schedulers.capacity_key_count_for_test(), 1);
    drop(first);

    let second = schedulers
        .reserve(Did::from(42_u32), TransferClass::Application, 1)
        .await
        .expect("second peer must reserve capacity");
    assert_eq!(schedulers.capacity_key_count_for_test(), 1);
    drop(second);
    schedulers.shutdown(Did::from(42_u32));

    assert_eq!(schedulers.capacity_key_count_for_test(), 0);
}
