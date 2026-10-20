//! Laws of outbound transfer capacity: reserved-class bounds over every short reserve/release
//! trace, reserved control bypassing borrower waiters, and FIFO global borrowing.

use std::sync::Arc;
use std::task::Poll;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use super::peer_byte_limit;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use super::transfer_limit;
use super::GlobalTransferCapacity;
use super::TransferCapacity;
use super::TransferClass;
use super::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
use super::OUTBOUND_GLOBAL_BYTE_CAPACITY;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use super::OUTBOUND_PEER_BYTE_CAPACITY;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use super::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::ReservedCapacity;

#[test]
fn test_reserved_capacity_bound_holds_for_all_short_reserve_release_traces() {
    const CAPACITY: usize = 5;
    const RESERVATIONS: [usize; TransferClass::COUNT] = [2, 1, 1, 1];
    const ACTIONS: usize = TransferClass::COUNT * 2;
    const TRACE_LENGTH: u32 = 6;
    let trace_count = ACTIONS.pow(TRACE_LENGTH);

    for encoded in 0..trace_count {
        let mut code = encoded;
        let mut capacity = ReservedCapacity::<{ TransferClass::COUNT }>::new();
        let mut admitted_by_class = [0usize; TransferClass::COUNT];
        for _ in 0..TRACE_LENGTH {
            let action = code % ACTIONS;
            code /= ACTIONS;
            let class_index = action % TransferClass::COUNT;
            if action < TransferClass::COUNT {
                if capacity.try_reserve(class_index, 1, CAPACITY, &RESERVATIONS) {
                    admitted_by_class[class_index] =
                        admitted_by_class[class_index].saturating_add(1);
                }
            } else if admitted_by_class[class_index] > 0 {
                capacity.release(class_index, 1);
                admitted_by_class[class_index] = admitted_by_class[class_index].saturating_sub(1);
            }
            assert_eq!(capacity.admitted(), admitted_by_class.iter().sum::<usize>());
            assert!(capacity.admitted() <= CAPACITY);
        }
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[test]
fn test_class_reservation_ablation_changes_real_admission_limits() {
    assert!(transfer_limit(TransferClass::Application) < OUTBOUND_TRANSFER_QUEUE_CAPACITY);
    assert!(peer_byte_limit(TransferClass::Application) < OUTBOUND_PEER_BYTE_CAPACITY);

    let _runtime = crate::simulation::SimulationRuntimeGuard::enter(
        42,
        1_700_000_000_000,
        crate::simulation::ProtectionProfile::without_class_reservations(),
    )
    .expect("simulation runtime must install");
    assert_eq!(
        transfer_limit(TransferClass::Application),
        OUTBOUND_TRANSFER_QUEUE_CAPACITY
    );
    assert_eq!(
        peer_byte_limit(TransferClass::Application),
        OUTBOUND_PEER_BYTE_CAPACITY
    );
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn test_reserved_outbound_request_bypasses_borrower_waiter() {
    let global = Arc::new(GlobalTransferCapacity::new());
    let capacity = Arc::new(TransferCapacity::new(global.clone()));
    let unrelated_capacity = Arc::new(TransferCapacity::new(global));
    let peer = Did::from(90_u32);
    let unrelated_peer = Did::from(91_u32);
    let blocker = capacity
        .try_acquire(peer, TransferClass::Application, 100 * 1024 * 1024)
        .expect("blocker must fit");
    let mut large = Box::pin(capacity.acquire(peer, TransferClass::Storage, 120 * 1024 * 1024));
    let mut unrelated = Box::pin(unrelated_capacity.acquire(
        unrelated_peer,
        TransferClass::Application,
        2 * 1024 * 1024,
    ));

    assert!(matches!(futures::poll!(large.as_mut()), Poll::Pending));
    let reserved = futures::future::join_all(
        (0..OUTBOUND_CONTROL_RESERVED_TRANSFERS)
            .map(|_| capacity.acquire(peer, TransferClass::DhtControl, 1)),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()
    .expect("control requests within their reservation must bypass");
    let mut later = Box::pin(capacity.acquire(peer, TransferClass::DhtControl, 1));
    assert!(matches!(
        futures::poll!(later.as_mut()),
        Poll::Ready(Err(_))
    ));
    assert!(matches!(
        futures::poll!(unrelated.as_mut()),
        Poll::Ready(Ok(_))
    ));
    drop(blocker);
    assert!(matches!(futures::poll!(large.as_mut()), Poll::Ready(Ok(_))));
    drop(reserved);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn test_global_borrowers_are_fifo_while_reserved_control_bypasses() {
    const BLOCKER_BYTES: usize = (OUTBOUND_GLOBAL_BYTE_CAPACITY - 16 * 1024 * 1024) / 2;
    let global = Arc::new(GlobalTransferCapacity::new());
    let first = Arc::new(TransferCapacity::new(global.clone()));
    let second = Arc::new(TransferCapacity::new(global.clone()));
    let waiting = Arc::new(TransferCapacity::new(global.clone()));
    let later = Arc::new(TransferCapacity::new(global.clone()));
    let control = Arc::new(TransferCapacity::new(global));
    let first_blocker = first
        .try_acquire(
            Did::from(101_u32),
            TransferClass::Application,
            BLOCKER_BYTES,
        )
        .expect("first global blocker must fit");
    let _second_blocker = second
        .try_acquire(
            Did::from(102_u32),
            TransferClass::Application,
            BLOCKER_BYTES,
        )
        .expect("second global blocker must fit");
    let mut front =
        Box::pin(waiting.acquire(Did::from(103_u32), TransferClass::Storage, 20 * 1024 * 1024));
    let mut behind =
        Box::pin(later.acquire(Did::from(104_u32), TransferClass::E2e, 8 * 1024 * 1024));
    let mut reserved = Box::pin(control.acquire(Did::from(105_u32), TransferClass::DhtControl, 1));

    assert!(matches!(futures::poll!(front.as_mut()), Poll::Pending));
    assert!(matches!(futures::poll!(behind.as_mut()), Poll::Pending));
    assert!(matches!(
        futures::poll!(reserved.as_mut()),
        Poll::Ready(Ok(_))
    ));
    drop(first_blocker);
    assert!(matches!(futures::poll!(front.as_mut()), Poll::Ready(Ok(_))));
    assert!(matches!(
        futures::poll!(behind.as_mut()),
        Poll::Ready(Ok(_))
    ));
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
async fn test_shared_wait_budget_bounds_payloads_outside_admitted_capacity() {
    const HEADROOM_BYTES: usize = 16 * 1024 * 1024;
    const WAIT_BYTES: usize = 20 * 1024 * 1024;
    let global = Arc::new(GlobalTransferCapacity::new());
    let first = Arc::new(TransferCapacity::new(global.clone()));
    let second = Arc::new(TransferCapacity::new(global.clone()));
    let blocker_bytes = (OUTBOUND_GLOBAL_BYTE_CAPACITY - HEADROOM_BYTES) / 2;
    let _first_blocker = first
        .try_acquire(
            Did::from(201_u32),
            TransferClass::Application,
            blocker_bytes,
        )
        .expect("first global blocker must fit");
    let _second_blocker = second
        .try_acquire(
            Did::from(202_u32),
            TransferClass::Application,
            blocker_bytes,
        )
        .expect("second global blocker must fit");
    let waiter_count = OUTBOUND_GLOBAL_BYTE_CAPACITY / WAIT_BYTES;
    let capacities = (0..=waiter_count)
        .map(|_| Arc::new(TransferCapacity::new(global.clone())))
        .collect::<Vec<_>>();
    let mut waiters = capacities
        .iter()
        .take(waiter_count)
        .enumerate()
        .map(|(index, capacity)| {
            Box::pin(capacity.acquire(
                Did::from(300_u32 + u32::try_from(index).expect("waiter index must fit in u32")),
                TransferClass::Storage,
                WAIT_BYTES,
            ))
        })
        .collect::<Vec<_>>();

    for waiter in &mut waiters {
        assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
    }
    let overflow_capacity = capacities.last().expect("overflow capacity must exist");
    let mut overflow =
        Box::pin(overflow_capacity.acquire(Did::from(400_u32), TransferClass::Storage, WAIT_BYTES));
    assert!(matches!(
        futures::poll!(overflow.as_mut()),
        Poll::Ready(Err(Error::OutboundTransferMemoryCapacityExceeded { .. }))
    ));
}
