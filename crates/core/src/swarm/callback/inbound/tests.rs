use std::task::Poll;

use super::*;
use crate::message::MessageClass;
use crate::utils::FairAdmission;

const DHT_CONTROL_LANE: InboundLane = InboundLane::from_class(MessageClass::DhtControl);
const STORAGE_LANE: InboundLane = InboundLane::from_class(MessageClass::Storage);
const E2E_LANE: InboundLane = InboundLane::from_class(MessageClass::E2e);
const APPLICATION_LANE: InboundLane = InboundLane::from_class(MessageClass::Application);
const REASSEMBLY_LANE: InboundLane = InboundLane::REASSEMBLY;

fn register_waiter(
    queue: &Arc<FairWaitQueue>,
    wake_counter: &Arc<WakeCounter>,
) -> crate::utils::FairWaiter {
    let FairAdmission::Waiting(mut waiter) = queue
        .admit_or_wait(1, (), || None::<()>)
        .expect("unbudgeted waiter must enqueue")
    else {
        panic!("blocked admission must return a waiter");
    };
    let waker = futures::task::waker_ref(wake_counter);
    let mut context = std::task::Context::from_waker(&waker);
    assert!(waiter
        .poll(&mut context, || None::<()>, |_| {})
        .is_pending());
    waiter
}

#[derive(Default)]
struct WakeCounter(std::sync::atomic::AtomicUsize);

impl futures::task::ArcWake for WakeCounter {
    fn wake_by_ref(counter: &Arc<Self>) {
        counter.0.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

#[test]
fn inbound_lane_mapping_is_total_and_reserves_one_extra_lane() {
    let lanes = [DHT_CONTROL_LANE, STORAGE_LANE, E2E_LANE, APPLICATION_LANE];

    assert_eq!(lanes.map(InboundLane::index), [0, 1, 2, 3]);
    assert_eq!(REASSEMBLY_LANE.index(), MessageClass::COUNT);
    assert!(!DHT_CONTROL_LANE.is_logical_data());
    assert!(!REASSEMBLY_LANE.is_logical_data());
    assert!(lanes
        .iter()
        .skip(1)
        .copied()
        .all(InboundLane::is_logical_data));
}

#[test]
fn inbound_waiter_wakeup_is_round_robin_and_bounded() {
    let peers = [
        Some(Did::from(1_u32)),
        Some(Did::from(2_u32)),
        Some(Did::from(3_u32)),
    ];
    let mut waiters = InboundWaitQueues::default();
    let queues = peers.map(|peer| waiters.queue_for_peer(peer));
    let counters: [Arc<WakeCounter>; 3] = std::array::from_fn(|_| Arc::new(WakeCounter::default()));
    let _registered = queues
        .iter()
        .zip(&counters)
        .map(|(queue, counter)| register_waiter(queue, counter))
        .collect::<Vec<_>>();

    let mut selected = Vec::new();
    let mut target = waiters
        .start_wake_round()
        .expect("one live queue must start the round");
    loop {
        let index = queues
            .iter()
            .position(|candidate| Arc::ptr_eq(candidate, &target.queue))
            .expect("selected queue must be registered");
        selected.push(index);
        assert!(target.queue.wake_front_with_handoff(target.remaining));
        let Some(next) = waiters.continue_wake_round(target.peer, target.remaining) else {
            break;
        };
        target = next;
    }
    assert_eq!(selected, vec![0, 1, 2]);
    assert_eq!(
        counters.map(|counter| counter.0.load(std::sync::atomic::Ordering::Acquire)),
        [1, 1, 1]
    );
}

#[test]
fn blocked_peers_complete_one_finite_wake_round() {
    let first_peer = Some(Did::from(1_u32));
    let second_peer = Some(Did::from(2_u32));
    let mut waiters = InboundWaitQueues::default();
    let first_queue = waiters.queue_for_peer(first_peer);
    let second_queue = waiters.queue_for_peer(second_peer);
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let mut first_waiter = register_waiter(&first_queue, &first_counter);
    let mut second_waiter = register_waiter(&second_queue, &second_counter);

    let selected = waiters
        .start_wake_round()
        .expect("the first peer must start the wake round");
    assert!(Arc::ptr_eq(&selected.queue, &first_queue));
    assert!(selected.queue.wake_front_with_handoff(selected.remaining));
    let waker = futures::task::waker_ref(&first_counter);
    let mut context = std::task::Context::from_waker(&waker);
    let mut first_handoff = None;
    assert!(first_waiter
        .poll(
            &mut context,
            || None::<()>,
            |handoff| first_handoff = Some(handoff)
        )
        .is_pending());
    let Some(FairHandoff::Continue(remaining)) = first_handoff else {
        panic!("a blocked peer must continue the current wake round");
    };
    let selected = waiters
        .continue_wake_round(first_peer, remaining)
        .expect("the second peer must receive the handoff");
    assert!(Arc::ptr_eq(&selected.queue, &second_queue));
    assert!(selected.queue.wake_front_with_handoff(selected.remaining));

    let waker = futures::task::waker_ref(&second_counter);
    let mut context = std::task::Context::from_waker(&waker);
    let mut second_handoff = None;
    assert!(second_waiter
        .poll(
            &mut context,
            || None::<()>,
            |handoff| {
                second_handoff = Some(handoff);
            }
        )
        .is_pending());
    let Some(FairHandoff::Continue(remaining)) = second_handoff else {
        panic!("the final blocked peer must exhaust the wake round");
    };
    assert!(waiters
        .continue_wake_round(second_peer, remaining)
        .is_none());
    assert_eq!(
        first_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );
}

#[test]
fn cancelled_selected_peer_continues_the_same_wake_round() {
    let capacity = Arc::new(InboundCapacity::new());
    let first_peer = Some(Did::from(1_u32));
    let second_peer = Some(Did::from(2_u32));
    let first_queue = capacity.waiters_for_peer(first_peer);
    let second_queue = capacity.waiters_for_peer(second_peer);
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let first_waiter = register_waiter(&first_queue, &first_counter);
    let _second_waiter = register_waiter(&second_queue, &second_counter);
    let selected = capacity
        .peer_waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .start_wake_round()
        .expect("the first peer must be selected before it cancels");
    assert!(Arc::ptr_eq(&selected.queue, &first_queue));

    drop(first_waiter);
    capacity.wake_waiter(Some(selected));

    assert_eq!(
        first_counter.0.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );
}

#[test]
fn successful_peer_restarts_wakeup_through_the_coordinator() {
    let first_peer = Some(Did::from(1_u32));
    let second_peer = Some(Did::from(2_u32));
    let mut waiters = InboundWaitQueues::default();
    let first_queue = waiters.queue_for_peer(first_peer);
    let second_queue = waiters.queue_for_peer(second_peer);
    let first_counter = Arc::new(WakeCounter::default());
    let same_peer_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let mut first_waiter = register_waiter(&first_queue, &first_counter);
    let _same_peer_waiter = register_waiter(&first_queue, &same_peer_counter);
    let _second_waiter = register_waiter(&second_queue, &second_counter);

    let selected = waiters
        .start_wake_round()
        .expect("the first peer must start the wake round");
    assert!(selected.queue.wake_front_with_handoff(selected.remaining));
    let waker = futures::task::waker_ref(&first_counter);
    let mut context = std::task::Context::from_waker(&waker);
    let mut handoff = None;
    assert!(first_waiter
        .poll(&mut context, || Some(()), |event| handoff = Some(event))
        .is_ready());
    assert!(matches!(handoff, Some(FairHandoff::Progress)));

    let selected = waiters
        .start_wake_round()
        .expect("progress must restart at the next peer");
    assert!(Arc::ptr_eq(&selected.queue, &second_queue));
    assert!(selected.queue.wake_front_with_handoff(selected.remaining));

    assert_eq!(
        same_peer_counter
            .0
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );
}

#[test]
fn inbound_waiter_rotation_skips_expired_queues() {
    let mut waiters = InboundWaitQueues::default();
    let expired = waiters.queue_for_peer(Some(Did::from(1_u32)));
    let live = waiters.queue_for_peer(Some(Did::from(2_u32)));
    drop(expired);

    let selected = waiters
        .start_wake_round()
        .expect("expired weak queue must not hide a live queue");
    assert!(Arc::ptr_eq(&selected.queue, &live));
    assert_eq!(waiters.len(), 1);
}

#[test]
fn inserting_unique_peers_prunes_expired_wait_queues() {
    let mut waiters = InboundWaitQueues::default();

    for peer in 1_u32..=64 {
        let queue = waiters.queue_for_peer(Some(Did::from(peer)));
        assert_eq!(waiters.len(), 1);
        drop(queue);
    }
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn small_unqueued_borrower_does_not_allocate_a_wait_queue() {
    let capacity = Arc::new(InboundCapacity::new());
    let peer = Some(Did::from(11_u32));
    let reserved = capacity
        .try_acquire(peer, APPLICATION_LANE, INBOUND_RESERVED_BYTES_PER_LANE)
        .expect("application reservation must fit");

    let borrowed = capacity
        .acquire(peer, APPLICATION_LANE, 1)
        .await
        .expect("small request may borrow shared capacity without queueing");

    assert_eq!(capacity.waiter_queue_count_for_test(), 0);
    drop(borrowed);
    drop(reserved);
}

fn reserve_application_bytes(
    capacity: &Arc<InboundCapacity>,
    total: usize,
) -> Vec<InboundCapacityPermit> {
    let mut remaining = total;
    let mut peer = 1_u32;
    let mut permits = Vec::new();
    while remaining > 0 {
        let bytes = remaining.min(INBOUND_PEER_BYTE_CAPACITY);
        permits.push(
            capacity
                .try_acquire(Some(Did::from(peer)), APPLICATION_LANE, bytes)
                .expect("application blocker must fit within the declared limits"),
        );
        remaining -= bytes;
        peer = peer.saturating_add(1);
    }
    permits
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn inbound_capacity_reserves_count_for_every_lane() {
    let capacity = Arc::new(InboundCapacity::new());
    let application_limit =
        INBOUND_MAILBOX_CAPACITY - INBOUND_RESERVED_TRANSFERS_PER_LANE * (INBOUND_LANE_COUNT - 1);
    let mut permits = (0..application_limit)
        .map(|index| {
            capacity.try_acquire(
                Some(Did::from(
                    u32::try_from(index + 1).expect("test peer index fits"),
                )),
                APPLICATION_LANE,
                1,
            )
        })
        .collect::<Result<Vec<_>>>()
        .expect("application may borrow capacity not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(u32::MAX)), APPLICATION_LANE, 1),
        Err(Error::InboundMailboxCapacityExceeded { .. })
    ));
    for (peer_index, lane) in [DHT_CONTROL_LANE, REASSEMBLY_LANE, STORAGE_LANE, E2E_LANE]
        .into_iter()
        .enumerate()
    {
        for _ in 0..INBOUND_RESERVED_TRANSFERS_PER_LANE {
            permits.push(
                capacity
                    .try_acquire(
                        Some(Did::from(
                            u32::try_from(application_limit + peer_index + 1)
                                .expect("test peer index fits"),
                        )),
                        lane,
                        1,
                    )
                    .expect("every lane retains its reserved admission capacity"),
            );
        }
    }
    assert!(matches!(
        capacity.try_acquire(Some(Did::from(u32::MAX)), DHT_CONTROL_LANE, 1),
        Err(Error::InboundMailboxCapacityExceeded { .. })
    ));
    drop(permits);
    assert!(capacity.try_acquire(None, APPLICATION_LANE, 1).is_ok());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn inbound_capacity_reserves_memory_for_every_lane() {
    let capacity = Arc::new(InboundCapacity::new());
    let application_limit =
        INBOUND_MAILBOX_BYTE_CAPACITY - INBOUND_RESERVED_BYTES_PER_LANE * (INBOUND_LANE_COUNT - 1);
    let first = capacity
        .try_acquire(
            Some(Did::from(1_u32)),
            APPLICATION_LANE,
            INBOUND_PEER_BYTE_CAPACITY,
        )
        .expect("one peer may retain one maximum-sized payload");
    let second = capacity
        .try_acquire(
            Some(Did::from(2_u32)),
            APPLICATION_LANE,
            application_limit - INBOUND_PEER_BYTE_CAPACITY,
        )
        .expect("application may borrow bytes not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(3_u32)), APPLICATION_LANE, 1),
        Err(Error::InboundMailboxMemoryCapacityExceeded { .. })
    ));
    let control = capacity
        .try_acquire(
            Some(Did::from(3_u32)),
            DHT_CONTROL_LANE,
            INBOUND_RESERVED_BYTES_PER_LANE,
        )
        .expect("control retains its reserved byte budget");
    drop(first);
    drop(second);
    drop(control);
    assert!(capacity.try_acquire(None, APPLICATION_LANE, 1).is_ok());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn reassembly_handoff_rejects_capacity_pressure_without_waiting() {
    let capacity = Arc::new(InboundCapacity::new());
    let blockers = reserve_application_bytes(&capacity, 240 * 1024 * 1024);
    let mut handoff = capacity
        .try_acquire(Some(Did::from(2_u32)), REASSEMBLY_LANE, 1)
        .expect("reassembly retains its reserved byte budget");

    assert!(matches!(
        handoff.try_transition(APPLICATION_LANE, 16 * 1024 * 1024),
        Err(Error::InboundMailboxMemoryCapacityExceeded { .. })
    ));
    assert_eq!(handoff.lane, REASSEMBLY_LANE);
    assert_eq!(handoff.bytes, 1);

    drop(blockers);
    handoff
        .try_transition(APPLICATION_LANE, 16 * 1024 * 1024)
        .expect("failed transition must preserve the original reservation atomically");
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn successful_capacity_transition_wakes_waiter_without_dropping_permit() {
    let capacity = Arc::new(InboundCapacity::new());
    let _blockers = reserve_application_bytes(&capacity, 240 * 1024 * 1024);
    let mut handoff = capacity
        .try_acquire(Some(Did::from(2_u32)), REASSEMBLY_LANE, 8 * 1024 * 1024)
        .expect("reassembly may borrow otherwise unused bytes");
    let mut waiter =
        Box::pin(capacity.acquire(Some(Did::from(3_u32)), STORAGE_LANE, 8 * 1024 * 1024));
    let wake_counter = Arc::new(WakeCounter::default());
    let waker = futures::task::waker_ref(&wake_counter);
    let mut context = std::task::Context::from_waker(&waker);
    assert!(std::future::Future::poll(waiter.as_mut(), &mut context).is_pending());
    assert_eq!(wake_counter.0.load(std::sync::atomic::Ordering::Acquire), 0);

    handoff
        .try_transition(REASSEMBLY_LANE, 1)
        .expect("shrinking the handoff reservation must succeed");
    assert_eq!(wake_counter.0.load(std::sync::atomic::Ordering::Acquire), 1);
    assert_eq!(handoff.bytes, 1);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn pending_ingress_ticket_blocks_later_same_lane_sequence() {
    let mut queues = InboundQueues::new();
    queues.push_pending(4, APPLICATION_LANE);
    queues.push_pending(5, APPLICATION_LANE);

    assert_eq!(queues.front_sequence(APPLICATION_LANE), Some(4));
    assert!(queues.pop(APPLICATION_LANE).is_none());

    queues.cancel(4, APPLICATION_LANE);
    assert_eq!(queues.front_sequence(APPLICATION_LANE), Some(5));
    assert!(queues.pop(APPLICATION_LANE).is_none());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn same_lane_ticket_preserves_capacity_admission_order_under_saturation() {
    let capacity = Arc::new(InboundCapacity::new());
    let blockers = reserve_application_bytes(&capacity, 240 * 1024 * 1024);
    let (command_sender, _commands) = mpsc::unbounded();
    let mut sender = InboundSender::new(command_sender);
    let mut first = sender
        .reserve(APPLICATION_LANE)
        .expect("first ticket must be reserved");
    let mut second = sender
        .reserve(APPLICATION_LANE)
        .expect("second ticket must be reserved");

    first.wait_for_admission_turn().await;
    let mut first_capacity =
        Box::pin(capacity.acquire(Some(Did::from(2_u32)), APPLICATION_LANE, 16 * 1024 * 1024));
    assert!(matches!(
        futures::poll!(first_capacity.as_mut()),
        Poll::Pending
    ));
    let mut second_turn = Box::pin(second.wait_for_admission_turn());
    assert!(matches!(
        futures::poll!(second_turn.as_mut()),
        Poll::Pending
    ));

    drop(blockers);
    assert!(matches!(
        futures::poll!(first_capacity.as_mut()),
        Poll::Ready(Ok(_))
    ));
    first.release_admission_turn();
    assert!(matches!(
        futures::poll!(second_turn.as_mut()),
        Poll::Ready(())
    ));
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn reserved_inbound_request_bypasses_borrower_waiter() {
    let capacity = Arc::new(InboundCapacity::new());
    let data_peer = Some(Did::from(1_u32));
    let control_peer = data_peer;
    let blocker = capacity
        .try_acquire(data_peer, APPLICATION_LANE, 100 * 1024 * 1024)
        .expect("blocker must fit");
    let mut large = Box::pin(capacity.acquire(data_peer, STORAGE_LANE, INBOUND_PEER_BYTE_CAPACITY));

    assert!(matches!(futures::poll!(large.as_mut()), Poll::Pending));
    let reserved = futures::future::join_all(
        (0..INBOUND_RESERVED_TRANSFERS_PER_LANE)
            .map(|_| capacity.acquire(control_peer, DHT_CONTROL_LANE, 1)),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()
    .expect("control requests within their reservation must bypass");
    let mut later = Box::pin(capacity.acquire(control_peer, DHT_CONTROL_LANE, 1));
    assert!(matches!(
        futures::poll!(later.as_mut()),
        Poll::Ready(Err(_))
    ));
    drop(reserved);
    drop(blocker);
    assert!(matches!(futures::poll!(large.as_mut()), Poll::Ready(Ok(_))));
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn peer_local_saturation_does_not_block_another_peers_large_request() {
    let capacity = Arc::new(InboundCapacity::new());
    let saturated_peer = Some(Did::from(1_u32));
    let available_peer = Some(Did::from(2_u32));
    let blocker = capacity
        .try_acquire(saturated_peer, APPLICATION_LANE, 120 * 1024 * 1024)
        .expect("peer-local blocker must fit");
    let mut blocked = Box::pin(capacity.acquire(saturated_peer, STORAGE_LANE, 16 * 1024 * 1024));
    assert!(matches!(futures::poll!(blocked.as_mut()), Poll::Pending));

    let available = capacity
        .acquire(available_peer, STORAGE_LANE, 16 * 1024 * 1024)
        .await
        .expect("a peer-local waiter must not block another peer");

    drop(blocker);
    assert!(matches!(
        futures::poll!(blocked.as_mut()),
        Poll::Ready(Ok(_))
    ));
    drop(available);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn one_peer_cannot_exhaust_another_peers_inbound_allowance() {
    let capacity = Arc::new(InboundCapacity::new());
    let noisy_peer = Some(Did::from(1_u32));
    let control_peer = Some(Did::from(2_u32));
    let permits = (0..INBOUND_PEER_CAPACITY)
        .map(|_| capacity.try_acquire(noisy_peer, APPLICATION_LANE, 1))
        .collect::<Result<Vec<_>>>()
        .expect("one peer may use exactly its own count allowance");

    assert!(matches!(
        capacity.try_acquire(noisy_peer, APPLICATION_LANE, 1),
        Err(Error::InboundPeerCapacityExceeded {
            peer,
            capacity: INBOUND_PEER_CAPACITY,
        }) if peer == noisy_peer
    ));
    assert!(capacity
        .try_acquire(control_peer, DHT_CONTROL_LANE, 1)
        .is_ok());
    drop(permits);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn inbound_callback_failures_preserve_typed_sources() {
    let validation = inbound_failure_error(InboundFailure::Validation(Box::new(
        std::io::Error::other("validation source"),
    )));
    let callback = inbound_failure_error(InboundFailure::Callback(Box::new(
        std::io::Error::other("callback source"),
    )));

    assert!(std::error::Error::source(&validation)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some());
    assert!(std::error::Error::source(&callback)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some());
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[test]
fn native_callback_error_contract_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<CallbackError>();
    let _: CallbackError = Box::new(std::io::Error::other("native callback error"));
}
