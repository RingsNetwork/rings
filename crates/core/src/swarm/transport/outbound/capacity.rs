use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;

use super::model::TransferClass;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::utils::fair_reservation_fits;
use crate::utils::fixed_reservation_covers;
use crate::utils::FairAdmission;
use crate::utils::FairWaitBudget;
use crate::utils::FairWaitQueue;

/// Hard per-peer transfer bound, including queued and delivery-waiting heads.
pub(crate) const OUTBOUND_TRANSFER_QUEUE_CAPACITY: usize = 256;
/// Slots unavailable to non-control transfers, so topology traffic can always enter the scheduler.
pub(crate) const OUTBOUND_CONTROL_RESERVED_TRANSFERS: usize = 16;
pub(super) const OUTBOUND_DATA_RESERVED_TRANSFERS: usize = 8;
/// Maximum non-control transfers admitted for one peer.
pub(crate) const OUTBOUND_DATA_TRANSFER_CAPACITY: usize = OUTBOUND_TRANSFER_QUEUE_CAPACITY
    - OUTBOUND_CONTROL_RESERVED_TRANSFERS
    - OUTBOUND_DATA_RESERVED_TRANSFERS * 2;
const OUTBOUND_TRANSFER_RESERVATIONS: [usize; TransferClass::COUNT] = [
    OUTBOUND_CONTROL_RESERVED_TRANSFERS,
    OUTBOUND_DATA_RESERVED_TRANSFERS,
    OUTBOUND_DATA_RESERVED_TRANSFERS,
    OUTBOUND_DATA_RESERVED_TRANSFERS,
];

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
    OUTBOUND_PEER_CONTROL_RESERVED_BYTES * 2,
    OUTBOUND_PEER_DATA_RESERVED_BYTES,
    OUTBOUND_PEER_DATA_RESERVED_BYTES,
    OUTBOUND_PEER_DATA_RESERVED_BYTES,
];

const fn peer_fixed_request_bytes(class: TransferClass) -> usize {
    match class {
        TransferClass::DhtControl => OUTBOUND_PEER_CONTROL_RESERVED_BYTES * 2,
        TransferClass::Storage | TransferClass::E2e | TransferClass::Application => {
            OUTBOUND_PEER_DATA_RESERVED_BYTES
        }
    }
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

const fn global_fixed_request_bytes(class: TransferClass) -> usize {
    match class {
        TransferClass::DhtControl => OUTBOUND_GLOBAL_CONTROL_RESERVED_BYTES,
        TransferClass::Storage | TransferClass::E2e | TransferClass::Application => {
            OUTBOUND_GLOBAL_DATA_RESERVED_BYTES
        }
    }
}

pub(super) struct GlobalTransferCapacity {
    state: Mutex<GlobalCapacityState>,
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
            state: Mutex::new(GlobalCapacityState::new()),
            waiters: Arc::new(FairWaitQueue::with_budget(wait_budget.clone())),
            wait_budget,
        }
    }

    fn try_acquire(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<GlobalCapacityPermit> {
        self.try_acquire_inner(peer, class, bytes, false)
    }

    fn try_acquire_reserved(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<GlobalCapacityPermit> {
        self.try_acquire_inner(peer, class, bytes, true)
    }

    fn try_acquire_inner(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
        reserved_only: bool,
    ) -> Result<GlobalCapacityPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = *state;
        if reserved_only && !next.reservation_covers(class, bytes) {
            return Err(memory_capacity_error(peer, bytes, global_byte_limit(class)));
        }
        if !next.try_reserve(class, bytes) {
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
        if let Ok(permit) = self.try_acquire_reserved(peer, class, bytes) {
            return Ok(permit);
        }
        if bytes <= global_fixed_request_bytes(class) {
            return self.waiters.try_admit_unqueued(
                memory_capacity_error(peer, bytes, global_byte_limit(class)),
                || self.try_acquire(peer, class, bytes),
            );
        }
        acquire_fair(
            &self.waiters,
            bytes,
            memory_capacity_error(peer, bytes, global_byte_limit(class)),
            || self.try_acquire(peer, class, bytes),
        )
        .await
    }
}

async fn acquire_fair<T>(
    queue: &Arc<FairWaitQueue>,
    cost: usize,
    budget_error: Error,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    match queue.admit_or_wait(cost, budget_error, &mut attempt)? {
        FairAdmission::Ready(value) => Ok(value),
        FairAdmission::Waiting(mut waiter) => {
            poll_fn(|context| match waiter.poll(context, || attempt().ok()) {
                Poll::Ready(Some(value)) => Poll::Ready(Ok(value)),
                Poll::Ready(None) => Poll::Ready(Err(Error::ChannelSendMessageFailed)),
                Poll::Pending => Poll::Pending,
            })
            .await
        }
    }
}

#[derive(Clone, Copy)]
struct GlobalCapacityState {
    admitted_bytes: usize,
    admitted_bytes_by_class: [usize; TransferClass::COUNT],
}

impl GlobalCapacityState {
    const fn new() -> Self {
        Self {
            admitted_bytes: 0,
            admitted_bytes_by_class: [0; TransferClass::COUNT],
        }
    }

    fn reservation_covers(&self, class: TransferClass, bytes: usize) -> bool {
        fixed_reservation_covers(
            &self.admitted_bytes_by_class,
            class.index(),
            bytes,
            &OUTBOUND_GLOBAL_BYTE_RESERVATIONS,
        )
    }

    fn try_reserve(&mut self, class: TransferClass, bytes: usize) -> bool {
        if !fair_reservation_fits(
            &self.admitted_bytes_by_class,
            self.admitted_bytes,
            class.index(),
            bytes,
            OUTBOUND_GLOBAL_BYTE_CAPACITY,
            &OUTBOUND_GLOBAL_BYTE_RESERVATIONS,
        ) {
            return false;
        }
        self.admitted_bytes += bytes;
        if let Some(class_bytes) = self.admitted_bytes_by_class.get_mut(class.index()) {
            *class_bytes += bytes;
        }
        true
    }

    fn release(&mut self, class: TransferClass, bytes: usize) {
        self.admitted_bytes = self.admitted_bytes.saturating_sub(bytes);
        if let Some(class_bytes) = self.admitted_bytes_by_class.get_mut(class.index()) {
            *class_bytes = class_bytes.saturating_sub(bytes);
        }
    }
}

#[derive(Clone, Copy)]
struct PeerCapacityState {
    admitted_transfers: usize,
    admitted_bytes: usize,
    admitted_transfers_by_class: [usize; TransferClass::COUNT],
    admitted_bytes_by_class: [usize; TransferClass::COUNT],
}

impl PeerCapacityState {
    const fn new() -> Self {
        Self {
            admitted_transfers: 0,
            admitted_bytes: 0,
            admitted_transfers_by_class: [0; TransferClass::COUNT],
            admitted_bytes_by_class: [0; TransferClass::COUNT],
        }
    }

    fn reservation_covers(&self, class: TransferClass, bytes: usize) -> bool {
        fixed_reservation_covers(
            &self.admitted_transfers_by_class,
            class.index(),
            1,
            &OUTBOUND_TRANSFER_RESERVATIONS,
        ) && fixed_reservation_covers(
            &self.admitted_bytes_by_class,
            class.index(),
            bytes,
            &OUTBOUND_PEER_BYTE_RESERVATIONS,
        )
    }

    fn try_reserve_count(&mut self, class: TransferClass) -> bool {
        if !fair_reservation_fits(
            &self.admitted_transfers_by_class,
            self.admitted_transfers,
            class.index(),
            1,
            OUTBOUND_TRANSFER_QUEUE_CAPACITY,
            &OUTBOUND_TRANSFER_RESERVATIONS,
        ) {
            return false;
        }
        self.admitted_transfers += 1;
        if let Some(class_count) = self.admitted_transfers_by_class.get_mut(class.index()) {
            *class_count += 1;
        }
        true
    }

    fn try_reserve_bytes(&mut self, class: TransferClass, bytes: usize) -> bool {
        if !fair_reservation_fits(
            &self.admitted_bytes_by_class,
            self.admitted_bytes,
            class.index(),
            bytes,
            OUTBOUND_PEER_BYTE_CAPACITY,
            &OUTBOUND_PEER_BYTE_RESERVATIONS,
        ) {
            return false;
        }
        self.admitted_bytes += bytes;
        if let Some(class_bytes) = self.admitted_bytes_by_class.get_mut(class.index()) {
            *class_bytes += bytes;
        }
        true
    }

    fn release(&mut self, class: TransferClass, bytes: usize) {
        self.admitted_transfers = self.admitted_transfers.saturating_sub(1);
        self.admitted_bytes = self.admitted_bytes.saturating_sub(bytes);
        if let Some(class_count) = self.admitted_transfers_by_class.get_mut(class.index()) {
            *class_count = class_count.saturating_sub(1);
        }
        if let Some(class_bytes) = self.admitted_bytes_by_class.get_mut(class.index()) {
            *class_bytes = class_bytes.saturating_sub(bytes);
        }
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

    fn try_acquire_peer(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<PeerCapacityPermit> {
        self.try_acquire_peer_inner(peer, class, bytes, false)
    }

    fn try_acquire_reserved_peer(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<PeerCapacityPermit> {
        self.try_acquire_peer_inner(peer, class, bytes, true)
    }

    fn try_acquire_peer_inner(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
        reserved_only: bool,
    ) -> Result<PeerCapacityPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = *state;
        if reserved_only && !next.reservation_covers(class, bytes) {
            return Err(memory_capacity_error(peer, bytes, peer_byte_limit(class)));
        }
        if !next.try_reserve_count(class) {
            return Err(Error::OutboundTransferCapacityExceeded {
                peer,
                capacity: transfer_limit(class),
            });
        }
        if !next.try_reserve_bytes(class, bytes) {
            return Err(memory_capacity_error(peer, bytes, peer_byte_limit(class)));
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
        if let Ok(permit) = self.try_acquire_reserved_peer(peer, class, bytes) {
            return Ok(permit);
        }
        if bytes <= peer_fixed_request_bytes(class) {
            return self.waiters.try_admit_unqueued(
                memory_capacity_error(peer, bytes, peer_byte_limit(class)),
                || self.try_acquire_peer(peer, class, bytes),
            );
        }
        acquire_fair(
            &self.waiters,
            bytes,
            memory_capacity_error(peer, bytes, peer_byte_limit(class)),
            || self.try_acquire_peer(peer, class, bytes),
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
        let peer_permit = self.try_acquire_peer(peer, class, bytes)?;
        let global_permit = self.global.try_acquire(peer, class, bytes)?;
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

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn admitted(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admitted_transfers
    }

    #[cfg(test)]
    pub(super) fn admitted_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admitted_bytes
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
            .release(self.class, self.bytes);
        self.capacity.waiters.wake_front();
    }
}

pub(super) const fn transfer_limit(class: TransferClass) -> usize {
    if class.is_control() {
        OUTBOUND_TRANSFER_QUEUE_CAPACITY - OUTBOUND_DATA_RESERVED_TRANSFERS * 3
    } else {
        OUTBOUND_DATA_TRANSFER_CAPACITY
    }
}

pub(super) const fn peer_byte_limit(class: TransferClass) -> usize {
    if class.is_control() {
        OUTBOUND_PEER_BYTE_CAPACITY - OUTBOUND_PEER_DATA_RESERVED_BYTES * 3
    } else {
        OUTBOUND_PEER_BYTE_CAPACITY
            - OUTBOUND_PEER_CONTROL_RESERVED_BYTES * 2
            - OUTBOUND_PEER_DATA_RESERVED_BYTES * 2
    }
}

const fn global_byte_limit(class: TransferClass) -> usize {
    if class.is_control() {
        OUTBOUND_GLOBAL_BYTE_CAPACITY - OUTBOUND_GLOBAL_DATA_RESERVED_BYTES * 3
    } else {
        OUTBOUND_GLOBAL_BYTE_CAPACITY
            - OUTBOUND_GLOBAL_CONTROL_RESERVED_BYTES
            - OUTBOUND_GLOBAL_DATA_RESERVED_BYTES * 2
    }
}

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
