use std::task::Poll;

use super::*;

#[derive(Default)]
struct WakeCounter(std::sync::atomic::AtomicUsize);

impl futures::task::ArcWake for WakeCounter {
    fn wake_by_ref(counter: &Arc<Self>) {
        counter.0.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
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
                .try_acquire(Some(Did::from(peer)), InboundLane::Application, bytes)
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
                InboundLane::Application,
                1,
            )
        })
        .collect::<Result<Vec<_>>>()
        .expect("application may borrow capacity not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(u32::MAX)), InboundLane::Application, 1),
        Err(Error::InboundMailboxCapacityExceeded { .. })
    ));
    for (peer_index, lane) in [
        InboundLane::DhtControl,
        InboundLane::Reassembly,
        InboundLane::Storage,
        InboundLane::E2e,
    ]
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
        capacity.try_acquire(Some(Did::from(u32::MAX)), InboundLane::DhtControl, 1),
        Err(Error::InboundMailboxCapacityExceeded { .. })
    ));
    drop(permits);
    assert!(capacity
        .try_acquire(None, InboundLane::Application, 1)
        .is_ok());
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
            InboundLane::Application,
            INBOUND_PEER_BYTE_CAPACITY,
        )
        .expect("one peer may retain one maximum-sized payload");
    let second = capacity
        .try_acquire(
            Some(Did::from(2_u32)),
            InboundLane::Application,
            application_limit - INBOUND_PEER_BYTE_CAPACITY,
        )
        .expect("application may borrow bytes not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(3_u32)), InboundLane::Application, 1),
        Err(Error::InboundMailboxMemoryCapacityExceeded { .. })
    ));
    let control = capacity
        .try_acquire(
            Some(Did::from(3_u32)),
            InboundLane::DhtControl,
            INBOUND_RESERVED_BYTES_PER_LANE,
        )
        .expect("control retains its reserved byte budget");
    drop(first);
    drop(second);
    drop(control);
    assert!(capacity
        .try_acquire(None, InboundLane::Application, 1)
        .is_ok());
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
        .try_acquire(Some(Did::from(2_u32)), InboundLane::Reassembly, 1)
        .expect("reassembly retains its reserved byte budget");

    assert!(matches!(
        handoff.try_transition(InboundLane::Application, 16 * 1024 * 1024),
        Err(Error::InboundMailboxMemoryCapacityExceeded { .. })
    ));
    assert_eq!(handoff.lane, InboundLane::Reassembly);
    assert_eq!(handoff.bytes, 1);

    drop(blockers);
    handoff
        .try_transition(InboundLane::Application, 16 * 1024 * 1024)
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
        .try_acquire(
            Some(Did::from(2_u32)),
            InboundLane::Reassembly,
            8 * 1024 * 1024,
        )
        .expect("reassembly may borrow otherwise unused bytes");
    let mut waiter = Box::pin(capacity.acquire(
        Some(Did::from(3_u32)),
        InboundLane::Storage,
        8 * 1024 * 1024,
    ));
    let wake_counter = Arc::new(WakeCounter::default());
    let waker = futures::task::waker_ref(&wake_counter);
    let mut context = std::task::Context::from_waker(&waker);
    assert!(std::future::Future::poll(waiter.as_mut(), &mut context).is_pending());
    assert_eq!(wake_counter.0.load(std::sync::atomic::Ordering::Acquire), 0);

    handoff
        .try_transition(InboundLane::Reassembly, 1)
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
    queues.push_pending(4, InboundLane::Application);
    queues.push_pending(5, InboundLane::Application);

    assert_eq!(queues.front_sequence(InboundLane::Application), Some(4));
    assert!(queues.pop(InboundLane::Application).is_none());

    queues.cancel(4, InboundLane::Application);
    assert_eq!(queues.front_sequence(InboundLane::Application), Some(5));
    assert!(queues.pop(InboundLane::Application).is_none());
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
        .reserve(InboundLane::Application)
        .expect("first ticket must be reserved");
    let mut second = sender
        .reserve(InboundLane::Application)
        .expect("second ticket must be reserved");

    first.wait_for_admission_turn().await;
    let mut first_capacity = Box::pin(capacity.acquire(
        Some(Did::from(2_u32)),
        InboundLane::Application,
        16 * 1024 * 1024,
    ));
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
        .try_acquire(data_peer, InboundLane::Application, 100 * 1024 * 1024)
        .expect("blocker must fit");
    let mut large =
        Box::pin(capacity.acquire(data_peer, InboundLane::Storage, INBOUND_PEER_BYTE_CAPACITY));

    assert!(matches!(futures::poll!(large.as_mut()), Poll::Pending));
    let reserved = futures::future::join_all(
        (0..INBOUND_RESERVED_TRANSFERS_PER_LANE)
            .map(|_| capacity.acquire(control_peer, InboundLane::DhtControl, 1)),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()
    .expect("control requests within their reservation must bypass");
    let mut later = Box::pin(capacity.acquire(control_peer, InboundLane::DhtControl, 1));
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
        .try_acquire(saturated_peer, InboundLane::Application, 120 * 1024 * 1024)
        .expect("peer-local blocker must fit");
    let mut blocked =
        Box::pin(capacity.acquire(saturated_peer, InboundLane::Storage, 16 * 1024 * 1024));
    assert!(matches!(futures::poll!(blocked.as_mut()), Poll::Pending));

    let available = capacity
        .acquire(available_peer, InboundLane::Storage, 16 * 1024 * 1024)
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
        .map(|_| capacity.try_acquire(noisy_peer, InboundLane::Application, 1))
        .collect::<Result<Vec<_>>>()
        .expect("one peer may use exactly its own count allowance");

    assert!(matches!(
        capacity.try_acquire(noisy_peer, InboundLane::Application, 1),
        Err(Error::InboundPeerCapacityExceeded {
            peer,
            capacity: INBOUND_PEER_CAPACITY,
        }) if peer == noisy_peer
    ));
    assert!(capacity
        .try_acquire(control_peer, InboundLane::DhtControl, 1)
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
