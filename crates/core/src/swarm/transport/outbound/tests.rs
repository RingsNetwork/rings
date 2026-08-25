use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::channel::oneshot;

use super::capacity::OUTBOUND_DATA_RESERVED_TRANSFERS;
use super::*;
use crate::ecc::SecretKey;
use crate::message::e2e::E2eHandshakeRequest;
use crate::message::Message;

type TestTransfer = (TransferClass, &'static str);

struct ShutdownProbe {
    admitted: Arc<AtomicUsize>,
    sender: Option<oneshot::Sender<usize>>,
}

impl Drop for ShutdownProbe {
    fn drop(&mut self) {
        let previous = self.admitted.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "a shutdown probe must own one admission");
    }
}

struct ShutdownProbeCompletion {
    admitted: Arc<AtomicUsize>,
    sender: oneshot::Sender<usize>,
}

struct ImplicitCompletionProbe {
    admitted: Arc<AtomicUsize>,
    sender: Option<oneshot::Sender<usize>>,
}

impl Drop for ImplicitCompletionProbe {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(self.admitted.load(Ordering::Acquire));
        }
    }
}

struct ImplicitCapacityProbe(Arc<AtomicUsize>);

impl Drop for ImplicitCapacityProbe {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(previous, 1, "the active transfer must own one admission");
    }
}

impl ShutdownProbeCompletion {
    fn publish(self) {
        let _ = self.sender.send(self.admitted.load(Ordering::Acquire));
    }
}

fn finalize_shutdown_probe(mut probe: ShutdownProbe) -> Option<ShutdownProbeCompletion> {
    let sender = probe.sender.take()?;
    let admitted = probe.admitted.clone();
    drop(probe);
    Some(ShutdownProbeCompletion { admitted, sender })
}

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

#[test]
fn detached_first_frame_success_and_cancellation_are_mutually_exclusive() {
    let cancelled = DetachedAdmission::new();
    let cancelled_stop = cancelled.stop_token();
    assert_eq!(cancelled.cancel(), DetachedAdmissionCancel::Cancelled);
    assert!(cancelled.try_mark_irrevocable().is_none());
    assert!(!cancelled.try_succeed());
    assert!(cancelled_stop.should_stop());

    let succeeded = DetachedAdmission::new();
    let succeeded_stop = succeeded.stop_token();
    assert!(succeeded.try_mark_irrevocable().is_some());
    assert!(succeeded.try_succeed());
    assert!(succeeded.try_mark_irrevocable().is_some());
    assert_eq!(succeeded.cancel(), DetachedAdmissionCancel::MustAwait);
    assert!(!succeeded_stop.should_stop());

    let irrevocable = DetachedAdmission::new();
    let irrevocable_stop = irrevocable.stop_token();
    assert!(irrevocable.try_mark_irrevocable().is_some());
    assert_eq!(irrevocable.cancel(), DetachedAdmissionCancel::MustAwait);
    assert!(!irrevocable_stop.should_stop());
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
fn every_transfer_class_uses_its_own_lane() {
    let classes = [
        TransferClass::DhtControl,
        TransferClass::Storage,
        TransferClass::E2e,
        TransferClass::Application,
    ];
    let mut queues = TransferQueues::default();
    for class in classes {
        push(&mut queues, class, "indexed-lane");
    }

    for _ in classes {
        let transfer = queues.pop().expect("every indexed lane must be reachable");
        assert_eq!(transfer.class(), transfer.item().0);
        queues.finish_current(transfer.class());
    }
}

#[test]
fn lower_cursor_wraps_and_skips_idle_lanes() {
    for (previous, expected) in [
        (TransferClass::Application, TransferClass::Storage),
        (TransferClass::Storage, TransferClass::E2e),
        (TransferClass::E2e, TransferClass::Application),
    ] {
        let mut queues = TransferQueues::default();
        queues.record_frame_admitted(previous);
        for class in [
            TransferClass::Storage,
            TransferClass::E2e,
            TransferClass::Application,
        ] {
            push(&mut queues, class, "round-robin");
        }
        assert_eq!(
            queues.pop().map(|transfer| transfer.class()),
            Some(expected)
        );
    }

    let mut sparse = TransferQueues::default();
    sparse.record_frame_admitted(TransferClass::Storage);
    push(&mut sparse, TransferClass::Application, "sparse");
    assert_eq!(
        sparse.pop().map(|transfer| transfer.class()),
        Some(TransferClass::Application)
    );
}

#[test]
fn terminal_attempt_advances_the_lower_class_cursor() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Storage, "stale-storage");
    push(&mut queues, TransferClass::E2e, "e2e");
    push(&mut queues, TransferClass::Application, "application");

    let stale = queues
        .pop()
        .expect("storage must start at the initial cursor");
    assert_eq!(stale.item().1, "stale-storage");
    queues.fail_attempt(stale);
    push(&mut queues, TransferClass::Storage, "more-stale-storage");

    let next = queues
        .pop()
        .expect("a failed attempt must yield to the next lower class");
    assert_eq!(next.item().1, "e2e");
}

#[test]
fn cancelled_control_attempts_consume_the_control_burst() {
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
        queues.fail_attempt(control);
    }
    push(&mut queues, TransferClass::DhtControl, "fifth-control");

    let next = queues
        .pop()
        .expect("lower work must run after failed control consumes the burst");
    assert_eq!(next.item().1, "application");
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
fn removing_ready_items_preserves_waiting_heads_and_fifo_order() {
    let mut queues = TransferQueues::default();
    push(&mut queues, TransferClass::Application, "waiting");
    push(&mut queues, TransferClass::Application, "cancel-1");
    push(&mut queues, TransferClass::Application, "keep");
    push(&mut queues, TransferClass::Application, "cancel-2");
    let waiting = queues.pop().expect("lane head must be runnable");
    queues.wait_for_delivery(17, waiting);

    let removed = queues.remove_ready_where(|(_, label)| label.starts_with("cancel"));

    assert_eq!(
        removed
            .into_iter()
            .map(|(_, label)| label)
            .collect::<Vec<_>>(),
        vec!["cancel-1", "cancel-2"]
    );
    let waiting = queues
        .take_waiting(TransferClass::Application, 17)
        .expect("active delivery must remain owned by the lane");
    queues.finish_current(TransferClass::Application);
    assert_eq!(waiting.item().1, "waiting");
    assert_eq!(pop_and_finish(&mut queues), Some("keep"));
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
    let (sender, mut receiver) = mailbox::channel();
    let stop = StopSource::new();
    let handle = OutboundPeerHandle {
        state: Arc::new(OutboundPeerState {
            #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
            peer: Did::from(42_u32),
            sender,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            _capacity_anchor: TransferCapacityAnchor::new(Arc::new(TransferCapacity::new(
                Arc::new(GlobalTransferCapacity::new()),
            ))),
            stop: stop.clone(),
        }),
    };

    handle.shutdown();

    assert!(stop.is_stop_requested());
    assert!(matches!(receiver.next().now_or_never(), Some(None)));
}

#[test]
fn worker_drop_stops_generation_and_closes_ingress_without_a_normal_run_exit() {
    let peer = Did::from(43_u32);
    let (sender, receiver) = mailbox::channel();
    let stop = StopSource::new();
    let (measurements, _measurement_receiver) = MeasurementRecorder::channel(None, peer);
    let worker = OutboundWorker::new(
        receiver,
        stop.clone(),
        measurements,
        peer,
        Arc::new(AtomicBool::new(false)),
    );

    drop(worker);

    assert!(stop.is_stop_requested());
    assert!(sender
        .send(OutboundCommand::CancelStopped, MailboxLane::Priority)
        .is_err());
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn worker_drop_allows_registry_to_replace_the_stopped_generation() {
    let schedulers = OutboundSchedulers::new(None);
    let peer = Did::from(44_u32);
    let capacity = schedulers
        .lock_registry()
        .capacity(peer, &schedulers.global_capacity);
    let (sender, receiver) = mailbox::channel();
    let stop = StopSource::new();
    let stale = OutboundPeerHandle {
        state: Arc::new(OutboundPeerState {
            #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
            peer,
            sender,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            _capacity_anchor: TransferCapacityAnchor::new(capacity),
            stop: stop.clone(),
        }),
    };
    schedulers.lock_registry().peers.insert(peer, stale.clone());
    let (measurements, _measurement_receiver) = MeasurementRecorder::channel(None, peer);
    let worker = OutboundWorker::new(
        receiver,
        stop,
        measurements,
        peer,
        Arc::new(AtomicBool::new(false)),
    );

    drop(worker);
    let replacement = schedulers
        .handle(peer)
        .expect("a stopped worker generation must be replaceable");

    assert!(!Arc::ptr_eq(&stale.state, &replacement.state));
    assert!(!replacement.state.stop.is_stop_requested());
    schedulers.shutdown(peer);
}

#[test]
fn shutdown_batch_finalizes_active_ready_and_buffered_before_first_publish() {
    let admitted = Arc::new(AtomicUsize::new(3));
    let (active_sender, active_receiver) = oneshot::channel();
    let (ready_sender, ready_receiver) = oneshot::channel();
    let (buffered_sender, buffered_receiver) = oneshot::channel();
    let active = ShutdownProbe {
        admitted: admitted.clone(),
        sender: Some(active_sender),
    };
    let ready = ShutdownProbe {
        admitted: admitted.clone(),
        sender: Some(ready_sender),
    };
    let buffered = ShutdownProbe {
        admitted: admitted.clone(),
        sender: Some(buffered_sender),
    };

    let completions = ShutdownBatch::new(vec![active], vec![ready], vec![buffered])
        .finalize(finalize_shutdown_probe);
    assert_eq!(admitted.load(Ordering::Acquire), 0);
    let mut completions = completions.into_iter();
    completions
        .next()
        .expect("active completion must be collected first")
        .publish();
    assert_eq!(active_receiver.now_or_never(), Some(Ok(0)));
    completions
        .next()
        .expect("ready completion must remain available")
        .publish();
    assert_eq!(ready_receiver.now_or_never(), Some(Ok(0)));
    completions
        .next()
        .expect("buffered completion must remain available")
        .publish();
    assert_eq!(buffered_receiver.now_or_never(), Some(Ok(0)));
    assert!(completions.next().is_none());
}

#[test]
fn active_transfer_drop_releases_capacity_before_implicit_completion() {
    let admitted = Arc::new(AtomicUsize::new(1));
    let (sender, receiver) = oneshot::channel();
    let scheduled = ScheduledTransfer::new(
        ImplicitCompletionProbe {
            admitted: admitted.clone(),
            sender: Some(sender),
        },
        ImplicitCapacityProbe(admitted.clone()),
    );

    drop(scheduled);

    assert_eq!(receiver.now_or_never(), Some(Ok(0)));
    assert_eq!(admitted.load(Ordering::Acquire), 0);
}

#[test]
fn final_handle_drop_requests_stop_before_channel_close() {
    let (sender, mut receiver) = mailbox::channel();
    let stop = StopSource::new();
    let handle = OutboundPeerHandle {
        state: Arc::new(OutboundPeerState {
            #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
            peer: Did::from(42_u32),
            sender,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            _capacity_anchor: TransferCapacityAnchor::new(Arc::new(TransferCapacity::new(
                Arc::new(GlobalTransferCapacity::new()),
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
                .reserve(peer, TransferClass::Application, 1)
                .expect("first generation must fill its declared capacity"),
        );
    }

    schedulers.shutdown(peer);
    let replacement = schedulers
        .handle(peer)
        .expect("replacement scheduler generation must start");

    assert!(matches!(
        replacement.reserve(peer, TransferClass::Application, 1),
        Err(Error::OutboundTransferCapacityExceeded {
            peer: error_peer,
            capacity: OUTBOUND_DATA_TRANSFER_CAPACITY,
        }) if error_peer == peer
    ));
    permits.pop();
    assert!(replacement
        .reserve(peer, TransferClass::Application, 1)
        .is_ok());
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

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn peer_state_keeps_idle_capacity_accountant_alive() {
    let schedulers = OutboundSchedulers::new(None);
    let peer = Did::from(43_u32);
    let handle = schedulers.handle(peer).expect("peer worker must start");
    let permit = schedulers
        .reserve(peer, TransferClass::Application, 1)
        .await
        .expect("peer must reserve capacity");

    drop(permit);
    assert_eq!(schedulers.capacity_key_count_for_test(), 1);

    drop(handle);
    schedulers.shutdown(peer);
    assert_eq!(schedulers.capacity_key_count_for_test(), 0);
}
