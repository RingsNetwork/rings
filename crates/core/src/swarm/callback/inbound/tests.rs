use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use super::*;
use crate::message::MessageClass;
use crate::utils::acquire_fair_with_handoff;
use crate::utils::FairWakeArm;
use crate::utils::FairWakeRound;

const DHT_CONTROL_LANE: InboundLane = InboundLane::from_class(MessageClass::DhtControl);
const STORAGE_LANE: InboundLane = InboundLane::from_class(MessageClass::Storage);
const E2E_LANE: InboundLane = InboundLane::from_class(MessageClass::E2e);
const APPLICATION_LANE: InboundLane = InboundLane::from_class(MessageClass::Application);
const REASSEMBLY_LANE: InboundLane = InboundLane::REASSEMBLY;

fn register_waiter<'a>(
    queue: &'a Arc<CoordinatedFairWaitQueue>,
    wake_counter: &Arc<WakeCounter>,
    handoffs: Arc<Mutex<Vec<FairHandoff>>>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    register_waiter_with_admission(
        queue,
        wake_counter,
        handoffs,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

fn register_waiter_with_admission<'a>(
    queue: &'a Arc<CoordinatedFairWaitQueue>,
    wake_counter: &Arc<WakeCounter>,
    handoffs: Arc<Mutex<Vec<FairHandoff>>>,
    admission: Arc<std::sync::atomic::AtomicBool>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    let mut waiter = Box::pin(acquire_fair_with_handoff(
        queue,
        FairCapacityDemand::new(APPLICATION_LANE.index(), 1),
        Error::InboundMailboxClosed,
        || Error::InboundMailboxClosed,
        move || {
            admission
                .load(std::sync::atomic::Ordering::Acquire)
                .then_some(())
                .ok_or(Error::InboundMailboxClosed)
        },
        move |handoff| {
            handoffs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(handoff);
        },
    ));
    let waker = futures::task::waker_ref(wake_counter);
    let mut context = Context::from_waker(&waker);
    assert!(waiter.as_mut().poll(&mut context).is_pending());
    waiter
}

fn take_handoff(handoffs: &Mutex<Vec<FairHandoff>>) -> FairHandoff {
    handoffs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop()
        .expect("the waiter must resolve one handoff event")
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
    let peers = [None, Some(Did::from(1_u32)), Some(Did::from(2_u32))];
    let mut waiters = InboundWaitQueues::default();
    let _queues = peers.map(|peer| waiters.queue_for_peer(peer));

    let mut selected = Vec::new();
    let mut target = waiters
        .request_wake_round()
        .expect("one live queue must start the round");
    loop {
        let index = peers
            .iter()
            .position(|candidate| *candidate == target.peer)
            .expect("selected queue must be registered");
        selected.push(index);
        let Some(next) = waiters.handle_handoff(
            target.peer,
            FairHandoff::Continue(target.round),
            AfterProgress::Stop,
        ) else {
            break;
        };
        target = next;
    }
    assert_eq!(selected, vec![0, 1, 2]);
}

#[test]
fn concurrent_release_repeats_one_serialized_round_after_exhaustion() {
    let peers = [Some(Did::from(1_u32)), Some(Did::from(2_u32))];
    let mut waiters = InboundWaitQueues::default();
    let _queues = peers.map(|peer| waiters.queue_for_peer(peer));

    let first = waiters
        .request_wake_round()
        .expect("the first peer must start the wake round");
    let first_round = first.round.clone();
    assert!(waiters.request_wake_round().is_none());
    let second = waiters
        .handle_handoff(
            first.peer,
            FairHandoff::Continue(first.round),
            AfterProgress::Stop,
        )
        .expect("the active round must continue to its second peer");
    assert_eq!(second.peer, peers[1]);
    assert_eq!(second.round, first_round);
    let repeated = waiters
        .handle_handoff(
            second.peer,
            FairHandoff::Continue(second.round),
            AfterProgress::Stop,
        )
        .expect("the concurrent release must request one fresh scan");
    assert_eq!(repeated.peer, peers[0]);
    assert_ne!(repeated.round, first_round);
    assert!(waiters
        .handle_handoff(
            first.peer,
            FairHandoff::Continue(first_round),
            AfterProgress::Stop,
        )
        .is_none());
}

#[test]
fn cancellation_after_handoff_arm_continues_the_same_wake_round() {
    let first_peer = Some(Did::from(1_u32));
    let second_peer = Some(Did::from(2_u32));
    let mut waiters = InboundWaitQueues::default();
    let first_queue = waiters.queue_for_peer(first_peer);
    let second_queue = waiters.queue_for_peer(second_peer);
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let handoffs = Arc::new(Mutex::new(Vec::new()));
    let first_waiter = register_waiter(&first_queue, &first_counter, handoffs.clone());
    let _second_waiter = register_waiter(
        &second_queue,
        &second_counter,
        Arc::new(Mutex::new(Vec::new())),
    );
    let selected = waiters
        .request_wake_round()
        .expect("the first peer must be selected before it cancels");
    let selected_round = selected.round.clone();
    assert_eq!(
        selected.queue.wake_front_with_handoff(selected.round),
        FairWakeArm::Armed
    );
    assert_eq!(
        first_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );

    drop(first_waiter);
    let handoff = take_handoff(&handoffs);
    let next = waiters
        .handle_handoff(first_peer, handoff, AfterProgress::Stop)
        .expect("cancellation must continue to the second peer");
    assert_eq!(next.peer, second_peer);
    assert_eq!(next.round, selected_round);
    assert_eq!(
        next.queue.wake_front_with_handoff(next.round),
        FairWakeArm::Armed
    );

    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );
}

#[test]
fn cancellation_before_handoff_arm_wakes_the_same_peer_successor() {
    let peer = Some(Did::from(1_u32));
    let mut waiters = InboundWaitQueues::default();
    let queue = waiters.queue_for_peer(peer);
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let first_handoffs = Arc::new(Mutex::new(Vec::new()));
    let first_waiter = register_waiter(&queue, &first_counter, first_handoffs.clone());
    let _second_waiter = register_waiter(&queue, &second_counter, Arc::new(Mutex::new(Vec::new())));

    drop(first_waiter);
    let target = waiters
        .handle_handoff(peer, take_handoff(&first_handoffs), AfterProgress::Stop)
        .expect("head cancellation must request a coordinated rescan");
    assert_eq!(target.peer, peer);
    assert_eq!(
        target.queue.wake_front_with_handoff(target.round),
        FairWakeArm::Armed
    );
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
fn blocked_poll_resolves_the_handoff_round_exactly_once() {
    let peer = Some(Did::from(1_u32));
    let mut waiters = InboundWaitQueues::default();
    let queue = waiters.queue_for_peer(peer);
    let counter = Arc::new(WakeCounter::default());
    let handoffs = Arc::new(Mutex::new(Vec::new()));
    let mut waiter = register_waiter(&queue, &counter, handoffs.clone());
    let target = waiters
        .request_wake_round()
        .expect("the blocked waiter must start a round");
    let round = target.round.clone();
    assert_eq!(
        target.queue.wake_front_with_handoff(target.round),
        FairWakeArm::Armed
    );

    let waker = futures::task::waker_ref(&counter);
    let mut context = Context::from_waker(&waker);
    assert!(waiter.as_mut().poll(&mut context).is_pending());
    let handoff = take_handoff(&handoffs);
    assert!(matches!(&handoff, FairHandoff::Continue(event) if event == &round));
    assert!(handoffs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
    assert!(waiters
        .handle_handoff(peer, handoff, AfterProgress::Stop)
        .is_none());
}

#[test]
fn successful_poll_resolves_the_handoff_round_exactly_once() {
    let peer = Some(Did::from(1_u32));
    let mut waiters = InboundWaitQueues::default();
    let queue = waiters.queue_for_peer(peer);
    let counter = Arc::new(WakeCounter::default());
    let handoffs = Arc::new(Mutex::new(Vec::new()));
    let admission = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut waiter =
        register_waiter_with_admission(&queue, &counter, handoffs.clone(), admission.clone());
    let target = waiters
        .request_wake_round()
        .expect("the blocked waiter must start a round");
    let round = target.round.clone();
    assert_eq!(
        target.queue.wake_front_with_handoff(target.round),
        FairWakeArm::Armed
    );
    admission.store(true, std::sync::atomic::Ordering::Release);

    let waker = futures::task::waker_ref(&counter);
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        waiter.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    let handoff = take_handoff(&handoffs);
    assert!(matches!(&handoff, FairHandoff::Progress(event) if event == &round));
    assert!(handoffs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
}

#[test]
fn successful_peer_restarts_wakeup_through_the_coordinator() {
    let first_peer = Some(Did::from(1_u32));
    let second_peer = Some(Did::from(2_u32));
    let mut waiters = InboundWaitQueues::default();
    let _first_queue = waiters.queue_for_peer(first_peer);
    let _second_queue = waiters.queue_for_peer(second_peer);

    let first = waiters
        .request_wake_round()
        .expect("the first peer must start the wake round");
    let first_round = first.round.clone();
    let selected = waiters
        .handle_handoff(
            first.peer,
            FairHandoff::Progress(first.round),
            AfterProgress::Scan,
        )
        .expect("progress must restart at the next peer");
    assert_eq!(selected.peer, second_peer);
    assert_ne!(selected.round, first_round);
}

#[test]
fn progress_with_consumed_capacity_closes_the_round_without_a_rescan() {
    let peers = [Some(Did::from(1_u32)), Some(Did::from(2_u32))];
    let mut waiters = InboundWaitQueues::default();
    let _queues = peers.map(|peer| waiters.queue_for_peer(peer));
    let first = waiters
        .request_wake_round()
        .expect("the first peer must start the wake round");
    let first_round = first.round.clone();
    assert!(waiters.request_wake_round().is_none());

    assert!(waiters
        .handle_handoff(
            first.peer,
            FairHandoff::Progress(first.round),
            AfterProgress::Stop,
        )
        .is_none());

    let next_release = waiters
        .request_wake_round()
        .expect("progress must close the round so the next release can start one");
    assert_eq!(next_release.peer, peers[1]);
    assert_ne!(next_release.round, first_round);
}

#[test]
fn already_armed_head_advances_the_new_round_without_parking_it() {
    let first_peer = Some(Did::from(1_u32));
    let second_peer = Some(Did::from(2_u32));
    let mut waiters = InboundWaitQueues::default();
    let first_queue = waiters.queue_for_peer(first_peer);
    let second_queue = waiters.queue_for_peer(second_peer);
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let first_handoffs = Arc::new(Mutex::new(Vec::new()));
    let second_handoffs = Arc::new(Mutex::new(Vec::new()));
    let first_waiter = register_waiter(&first_queue, &first_counter, first_handoffs.clone());
    let mut second_waiter =
        register_waiter(&second_queue, &second_counter, second_handoffs.clone());
    let stale_round = FairWakeRound::new(u64::MAX);
    assert_eq!(
        first_queue.wake_front_with_handoff(stale_round),
        FairWakeArm::Armed
    );

    let first_target = waiters
        .request_wake_round()
        .expect("the new round must select the already-armed peer first");
    assert_eq!(first_target.peer, first_peer);
    let current_round = first_target.round.clone();
    let waiters = Mutex::new(waiters);
    wake_waiter(&waiters, Some(first_target));
    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );

    drop(first_waiter);
    assert!(waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .handle_handoff(
            first_peer,
            take_handoff(&first_handoffs),
            AfterProgress::Stop,
        )
        .is_none());
    let waker = futures::task::waker_ref(&second_counter);
    let mut context = Context::from_waker(&waker);
    assert!(second_waiter.as_mut().poll(&mut context).is_pending());
    let handoff = take_handoff(&second_handoffs);
    assert!(matches!(&handoff, FairHandoff::Continue(round) if round == &current_round));
    assert!(waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .handle_handoff(second_peer, handoff, AfterProgress::Stop,)
        .is_none());

    assert!(waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .request_wake_round()
        .is_some());
}

#[test]
fn active_round_skips_a_queue_that_expires_mid_scan() {
    let peers = [
        Some(Did::from(1_u32)),
        Some(Did::from(2_u32)),
        Some(Did::from(3_u32)),
    ];
    let mut waiters = InboundWaitQueues::default();
    let _first = waiters.queue_for_peer(peers[0]);
    let expired = waiters.queue_for_peer(peers[1]);
    let third = waiters.queue_for_peer(peers[2]);
    let selected = waiters
        .request_wake_round()
        .expect("the first peer must start the wake round");
    drop(expired);

    let next = waiters
        .handle_handoff(
            selected.peer,
            FairHandoff::Continue(selected.round),
            AfterProgress::Stop,
        )
        .expect("expiry must not hide the remaining live peer");
    assert_eq!(next.peer, peers[2]);
    assert!(Arc::ptr_eq(&next.queue, &third));
}

#[test]
fn inbound_waiter_rotation_skips_expired_queues() {
    let mut waiters = InboundWaitQueues::default();
    let expired = waiters.queue_for_peer(Some(Did::from(1_u32)));
    let live = waiters.queue_for_peer(Some(Did::from(2_u32)));
    drop(expired);

    let selected = waiters
        .request_wake_round()
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
async fn one_large_release_can_admit_multiple_smaller_waiters() {
    let capacity = Arc::new(InboundCapacity::new());
    let mut blockers = reserve_application_bytes(&capacity, 240 * 1024 * 1024).into_iter();
    let released = blockers
        .next()
        .expect("the saturated mailbox must retain one large permit");
    let _remaining_blockers = blockers.collect::<Vec<_>>();
    let mut first =
        Box::pin(capacity.acquire(Some(Did::from(3_u32)), STORAGE_LANE, 16 * 1024 * 1024));
    let mut second =
        Box::pin(capacity.acquire(Some(Did::from(4_u32)), STORAGE_LANE, 16 * 1024 * 1024));
    assert!(matches!(futures::poll!(first.as_mut()), Poll::Pending));
    assert!(matches!(futures::poll!(second.as_mut()), Poll::Pending));

    drop(released);

    let Poll::Ready(Ok(first_permit)) = futures::poll!(first.as_mut()) else {
        panic!("the first small waiter must consume part of the large release");
    };
    let Poll::Ready(Ok(second_permit)) = futures::poll!(second.as_mut()) else {
        panic!("the remaining released bytes must admit the second waiter");
    };
    drop(second_permit);
    drop(first_permit);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn exact_byte_replacement_does_not_start_a_failed_peer_scan() {
    let capacity = Arc::new(InboundCapacity::new());
    let _storage_blockers = [
        (Some(Did::from(1_u32)), 128 * 1024 * 1024),
        (Some(Did::from(2_u32)), 122 * 1024 * 1024),
    ]
    .map(|(peer, bytes)| {
        capacity
            .try_acquire(peer, STORAGE_LANE, bytes)
            .expect("the storage blocker must fit")
    });
    let released = capacity
        .try_acquire(Some(Did::from(3_u32)), STORAGE_LANE, 2 * 1024 * 1024)
        .expect("the releasable storage blocker must fit");
    let mut first =
        Box::pin(capacity.acquire(Some(Did::from(4_u32)), STORAGE_LANE, 2 * 1024 * 1024));
    let mut second =
        Box::pin(capacity.acquire(Some(Did::from(5_u32)), STORAGE_LANE, 2 * 1024 * 1024));
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let first_waker = futures::task::waker_ref(&first_counter);
    let second_waker = futures::task::waker_ref(&second_counter);
    let mut first_context = Context::from_waker(&first_waker);
    let mut second_context = Context::from_waker(&second_waker);
    assert!(first.as_mut().poll(&mut first_context).is_pending());
    assert!(second.as_mut().poll(&mut second_context).is_pending());

    drop(released);
    let Poll::Ready(Ok(first_permit)) = first.as_mut().poll(&mut first_context) else {
        panic!("the first waiter must replace the released byte capacity");
    };

    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    drop(first_permit);
    assert_eq!(
        second_counter.0.load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert!(matches!(
        second.as_mut().poll(&mut second_context),
        Poll::Ready(Ok(_))
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
