use std::sync::Arc;
use std::sync::Mutex;
#[cfg(test)]
use std::task::Poll;

use super::model::TransferClass;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::acquire_fair;
use crate::fair_admission::admissible_capacity;
use crate::fair_admission::retained_wire_bytes;
use crate::fair_admission::CountedReservationRejection;
use crate::fair_admission::CountedReservedCapacity;
use crate::fair_admission::FairWaitBudget;
use crate::fair_admission::FairWaitQueue;
use crate::fair_admission::ReservedCapacity;

/// Hard per-peer transfer bound, including queued and delivery-waiting heads.
pub(crate) const OUTBOUND_TRANSFER_QUEUE_CAPACITY: usize = 256;
/// Slots unavailable to non-control transfers, so topology traffic can always enter the scheduler.
pub(crate) const OUTBOUND_CONTROL_RESERVED_TRANSFERS: usize = 16;
pub(super) const OUTBOUND_DATA_RESERVED_TRANSFERS: usize = 8;
/// Per-class minimum transfer reservations preserved under shared-capacity borrowing.
const OUTBOUND_TRANSFER_RESERVATIONS: [usize; TransferClass::COUNT] = [
    OUTBOUND_CONTROL_RESERVED_TRANSFERS,
    OUTBOUND_DATA_RESERVED_TRANSFERS,
    OUTBOUND_DATA_RESERVED_TRANSFERS,
    OUTBOUND_DATA_RESERVED_TRANSFERS,
];
pub(crate) const OUTBOUND_DATA_TRANSFER_CAPACITY: usize = admissible_capacity(
    OUTBOUND_TRANSFER_QUEUE_CAPACITY,
    &OUTBOUND_TRANSFER_RESERVATIONS,
    TransferClass::Application.index(),
);

/// Weighted outbound memory allowed for one peer.
///
/// Preparation charges twice the exact wire size, covering the owned payload
/// plus either its decoded message body or serialized wire copy. This capacity
/// therefore admits one maximum-sized payload while bounding retained queues to
/// roughly 64 MiB of wire data.
pub(super) const OUTBOUND_PEER_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
/// Per-peer bytes unavailable to non-control traffic.
pub(super) const OUTBOUND_PEER_CONTROL_RESERVED_BYTES: usize = 1024 * 1024;
pub(super) const OUTBOUND_PEER_DATA_RESERVED_BYTES: usize = 1024 * 1024;
const OUTBOUND_PEER_BYTE_RESERVATIONS: [usize; TransferClass::COUNT] = [
    retained_wire_bytes(OUTBOUND_PEER_CONTROL_RESERVED_BYTES),
    OUTBOUND_PEER_DATA_RESERVED_BYTES,
    OUTBOUND_PEER_DATA_RESERVED_BYTES,
    OUTBOUND_PEER_DATA_RESERVED_BYTES,
];

fn fixed_request_bytes(
    reservations: &[usize; TransferClass::COUNT],
    class: TransferClass,
) -> usize {
    reservations.get(class.index()).copied().unwrap_or(0)
}

/// Native-wide retained outbound bytes across all peers.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
const OUTBOUND_GLOBAL_BYTE_CAPACITY: usize = 256 * 1024 * 1024;
/// Browser-wide retained outbound bytes across all peers.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
const OUTBOUND_GLOBAL_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
/// Global bytes unavailable to non-control traffic.
const OUTBOUND_GLOBAL_CONTROL_RESERVED_BYTES: usize = 2 * 1024 * 1024;
const OUTBOUND_GLOBAL_DATA_RESERVED_BYTES: usize = 1024 * 1024;
const OUTBOUND_PENDING_TRANSFER_CAPACITY: usize = OUTBOUND_TRANSFER_QUEUE_CAPACITY;
const OUTBOUND_GLOBAL_BYTE_RESERVATIONS: [usize; TransferClass::COUNT] = [
    OUTBOUND_GLOBAL_CONTROL_RESERVED_BYTES,
    OUTBOUND_GLOBAL_DATA_RESERVED_BYTES,
    OUTBOUND_GLOBAL_DATA_RESERVED_BYTES,
    OUTBOUND_GLOBAL_DATA_RESERVED_BYTES,
];

#[derive(Clone, Copy, Eq, PartialEq)]
enum CapacityScope {
    FixedReservation,
    Shared,
}

async fn acquire_with_fixed_reservation<T>(
    waiters: &Arc<FairWaitQueue>,
    bytes: usize,
    fixed_request_limit: usize,
    capacity_error: impl Fn() -> Error,
    try_reserved: impl FnOnce() -> Result<T>,
    mut try_shared: impl FnMut() -> Result<T>,
) -> Result<T> {
    if let Ok(permit) = try_reserved() {
        return Ok(permit);
    }
    if bytes <= fixed_request_limit {
        return waiters.try_admit_unqueued(capacity_error(), try_shared);
    }
    acquire_fair(
        waiters,
        bytes,
        capacity_error(),
        || Error::ChannelSendMessageFailed,
        || try_shared().ok(),
    )
    .await
}

pub(super) struct GlobalTransferCapacity {
    state: Mutex<ReservedCapacity<{ TransferClass::COUNT }>>,
    waiters: Arc<FairWaitQueue>,
    wait_budget: Arc<FairWaitBudget>,
}

impl GlobalTransferCapacity {
    pub(super) fn new() -> Self {
        let wait_budget = Arc::new(FairWaitBudget::new(
            OUTBOUND_PENDING_TRANSFER_CAPACITY,
            OUTBOUND_GLOBAL_BYTE_CAPACITY,
        ));
        Self {
            state: Mutex::new(ReservedCapacity::new()),
            waiters: Arc::new(FairWaitQueue::with_budget(wait_budget.clone())),
            wait_budget,
        }
    }

    fn try_acquire_inner(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
        scope: CapacityScope,
    ) -> Result<GlobalCapacityPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = *state;
        if scope == CapacityScope::FixedReservation
            && !next.reservation_covers(class.index(), bytes, &OUTBOUND_GLOBAL_BYTE_RESERVATIONS)
        {
            return Err(memory_capacity_error(peer, bytes, global_byte_limit(class)));
        }
        if !next.try_reserve(
            class.index(),
            bytes,
            OUTBOUND_GLOBAL_BYTE_CAPACITY,
            &OUTBOUND_GLOBAL_BYTE_RESERVATIONS,
        ) {
            return Err(memory_capacity_error(peer, bytes, global_byte_limit(class)));
        }
        *state = next;
        Ok(GlobalCapacityPermit {
            capacity: self.clone(),
            class,
            bytes,
        })
    }

    async fn acquire(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<GlobalCapacityPermit> {
        acquire_with_fixed_reservation(
            &self.waiters,
            bytes,
            fixed_request_bytes(&OUTBOUND_GLOBAL_BYTE_RESERVATIONS, class),
            || memory_capacity_error(peer, bytes, global_byte_limit(class)),
            || self.try_acquire_inner(peer, class, bytes, CapacityScope::FixedReservation),
            || self.try_acquire_inner(peer, class, bytes, CapacityScope::Shared),
        )
        .await
    }
}

#[derive(Clone, Copy)]
struct PeerCapacityState {
    capacity: CountedReservedCapacity<{ TransferClass::COUNT }>,
}

impl PeerCapacityState {
    const fn new() -> Self {
        Self {
            capacity: CountedReservedCapacity::new(),
        }
    }

    fn reservation_covers(&self, class: TransferClass, bytes: usize) -> bool {
        self.capacity.reservation_covers(
            class.index(),
            bytes,
            &OUTBOUND_TRANSFER_RESERVATIONS,
            &OUTBOUND_PEER_BYTE_RESERVATIONS,
        )
    }

    fn try_reserve(
        &mut self,
        class: TransferClass,
        bytes: usize,
    ) -> std::result::Result<(), CountedReservationRejection> {
        self.capacity.try_reserve(
            class.index(),
            bytes,
            OUTBOUND_TRANSFER_QUEUE_CAPACITY,
            &OUTBOUND_TRANSFER_RESERVATIONS,
            OUTBOUND_PEER_BYTE_CAPACITY,
            &OUTBOUND_PEER_BYTE_RESERVATIONS,
        )
    }

    fn release(&mut self, class: TransferClass, bytes: usize) {
        self.capacity.release(class.index(), bytes);
    }
}

pub(super) struct TransferCapacity {
    state: Mutex<PeerCapacityState>,
    global: Arc<GlobalTransferCapacity>,
    waiters: Arc<FairWaitQueue>,
}

impl TransferCapacity {
    pub(super) fn new(global: Arc<GlobalTransferCapacity>) -> Self {
        let wait_budget = global.wait_budget.clone();
        Self {
            state: Mutex::new(PeerCapacityState::new()),
            global,
            waiters: Arc::new(FairWaitQueue::with_budget(wait_budget)),
        }
    }

    fn try_acquire_peer_inner(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
        scope: CapacityScope,
    ) -> Result<PeerCapacityPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = *state;
        if scope == CapacityScope::FixedReservation && !next.reservation_covers(class, bytes) {
            return Err(memory_capacity_error(peer, bytes, peer_byte_limit(class)));
        }
        match next.try_reserve(class, bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::OutboundTransferCapacityExceeded {
                    peer,
                    capacity: transfer_limit(class),
                });
            }
            Err(CountedReservationRejection::Bytes) => {
                return Err(memory_capacity_error(peer, bytes, peer_byte_limit(class)));
            }
        }
        *state = next;
        Ok(PeerCapacityPermit {
            capacity: self.clone(),
            class,
            bytes,
        })
    }

    async fn acquire_peer(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<PeerCapacityPermit> {
        acquire_with_fixed_reservation(
            &self.waiters,
            bytes,
            fixed_request_bytes(&OUTBOUND_PEER_BYTE_RESERVATIONS, class),
            || memory_capacity_error(peer, bytes, peer_byte_limit(class)),
            || self.try_acquire_peer_inner(peer, class, bytes, CapacityScope::FixedReservation),
            || self.try_acquire_peer_inner(peer, class, bytes, CapacityScope::Shared),
        )
        .await
    }

    #[cfg(test)]
    pub(super) fn try_acquire(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        validate_memory_request(peer, class, bytes)?;
        let bytes = bytes.max(1);
        let peer_permit = self.try_acquire_peer_inner(peer, class, bytes, CapacityScope::Shared)?;
        let global_permit =
            self.global
                .try_acquire_inner(peer, class, bytes, CapacityScope::Shared)?;
        Ok(TransferCapacityPermit {
            _peer: peer_permit,
            _global: global_permit,
        })
    }

    pub(super) async fn acquire(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        validate_memory_request(peer, class, bytes)?;
        let bytes = bytes.max(1);
        let peer_permit = self.acquire_peer(peer, class, bytes).await?;
        let global_permit = self.global.acquire(peer, class, bytes).await?;
        Ok(TransferCapacityPermit {
            _peer: peer_permit,
            _global: global_permit,
        })
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn admitted(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capacity
            .admitted_count()
    }

    #[cfg(test)]
    pub(super) fn admitted_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capacity
            .admitted_bytes()
    }
}

pub(in crate::swarm::transport) struct TransferCapacityPermit {
    _peer: PeerCapacityPermit,
    _global: GlobalCapacityPermit,
}

struct PeerCapacityPermit {
    capacity: Arc<TransferCapacity>,
    class: TransferClass,
    bytes: usize,
}

impl Drop for PeerCapacityPermit {
    fn drop(&mut self) {
        self.capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release(self.class, self.bytes);
        self.capacity.waiters.wake_front();
    }
}

struct GlobalCapacityPermit {
    capacity: Arc<GlobalTransferCapacity>,
    class: TransferClass,
    bytes: usize,
}

impl Drop for GlobalCapacityPermit {
    fn drop(&mut self) {
        self.capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release(self.class.index(), self.bytes);
        self.capacity.waiters.wake_front();
    }
}

pub(super) const fn transfer_limit(class: TransferClass) -> usize {
    match class {
        TransferClass::DhtControl => admissible_capacity(
            OUTBOUND_TRANSFER_QUEUE_CAPACITY,
            &OUTBOUND_TRANSFER_RESERVATIONS,
            class.index(),
        ),
        TransferClass::Storage | TransferClass::E2e | TransferClass::Application => {
            OUTBOUND_DATA_TRANSFER_CAPACITY
        }
    }
}

pub(super) const fn peer_byte_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_PEER_BYTE_CAPACITY,
        &OUTBOUND_PEER_BYTE_RESERVATIONS,
        class.index(),
    )
}

const fn global_byte_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_GLOBAL_BYTE_CAPACITY,
        &OUTBOUND_GLOBAL_BYTE_RESERVATIONS,
        class.index(),
    )
}

const _: () = {
    let maximum_payload_reservation = retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE);
    assert!(maximum_payload_reservation <= peer_byte_limit(TransferClass::DhtControl));
    assert!(maximum_payload_reservation <= peer_byte_limit(TransferClass::Application));
    assert!(maximum_payload_reservation <= global_byte_limit(TransferClass::DhtControl));
    assert!(maximum_payload_reservation <= global_byte_limit(TransferClass::Application));
};

fn memory_capacity_error(peer: Did, requested_bytes: usize, capacity_bytes: usize) -> Error {
    Error::OutboundTransferMemoryCapacityExceeded {
        peer,
        requested_bytes,
        capacity_bytes,
    }
}

fn validate_memory_request(peer: Did, class: TransferClass, requested_bytes: usize) -> Result<()> {
    let requested_bytes = requested_bytes.max(1);
    let limit = peer_byte_limit(class).min(global_byte_limit(class));
    if requested_bytes > limit {
        return Err(memory_capacity_error(peer, requested_bytes, limit));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_capacity_bound_holds_for_all_short_reserve_release_traces() {
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
                    admitted_by_class[class_index] =
                        admitted_by_class[class_index].saturating_sub(1);
                }
                assert_eq!(capacity.admitted(), admitted_by_class.iter().sum::<usize>());
                assert!(capacity.admitted() <= CAPACITY);
            }
        }
    }

    #[cfg_attr(
        all(feature = "wasm", target_family = "wasm"),
        wasm_bindgen_test::wasm_bindgen_test
    )]
    #[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
    async fn reserved_outbound_request_bypasses_borrower_waiter() {
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
    async fn global_borrowers_are_fifo_while_reserved_control_bypasses() {
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
        let mut reserved =
            Box::pin(control.acquire(Did::from(105_u32), TransferClass::DhtControl, 1));

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
    async fn shared_wait_budget_bounds_payloads_outside_admitted_capacity() {
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
                    Did::from(
                        300_u32 + u32::try_from(index).expect("waiter index must fit in u32"),
                    ),
                    TransferClass::Storage,
                    WAIT_BYTES,
                ))
            })
            .collect::<Vec<_>>();

        for waiter in &mut waiters {
            assert!(matches!(futures::poll!(waiter.as_mut()), Poll::Pending));
        }
        let overflow_capacity = capacities.last().expect("overflow capacity must exist");
        let mut overflow = Box::pin(overflow_capacity.acquire(
            Did::from(400_u32),
            TransferClass::Storage,
            WAIT_BYTES,
        ));
        assert!(matches!(
            futures::poll!(overflow.as_mut()),
            Poll::Ready(Err(Error::OutboundTransferMemoryCapacityExceeded { .. }))
        ));
    }
}
