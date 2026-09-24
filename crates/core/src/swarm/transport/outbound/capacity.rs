use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
#[cfg(test)]
use std::task::Poll;

use event_listener::EventListener;

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
use crate::lifecycle::epoch::Epoch;

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
#[cfg(test)]
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
pub(crate) const OUTBOUND_GLOBAL_BYTE_CAPACITY: usize = 256 * 1024 * 1024;
/// Browser-wide retained outbound bytes across all peers.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) const OUTBOUND_GLOBAL_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
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
const NO_RESERVATIONS: [usize; TransferClass::COUNT] = [0; TransferClass::COUNT];

fn class_reservations_enabled() -> bool {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    {
        crate::simulation::protection_profile().class_reservations()
    }
    #[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
    {
        true
    }
}

fn active_reservations(
    reservations: &'static [usize; TransferClass::COUNT],
) -> &'static [usize; TransferClass::COUNT] {
    if class_reservations_enabled() {
        reservations
    } else {
        &NO_RESERVATIONS
    }
}

fn transfer_reservations() -> &'static [usize; TransferClass::COUNT] {
    active_reservations(&OUTBOUND_TRANSFER_RESERVATIONS)
}

fn peer_byte_reservations() -> &'static [usize; TransferClass::COUNT] {
    active_reservations(&OUTBOUND_PEER_BYTE_RESERVATIONS)
}

fn global_byte_reservations() -> &'static [usize; TransferClass::COUNT] {
    active_reservations(&OUTBOUND_GLOBAL_BYTE_RESERVATIONS)
}

/// The two ways `acquire_with_fixed_reservation` can admit a demand.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::swarm::transport) enum CapacityScope {
    /// The class's fixed reservation, open whatever is queued.
    FixedReservation,
    /// The shared capacity, reached only through the fair-wait queue.
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

/// `Admits ≜ reserved(Fixed) ∨ (queue empty ∧ reserved(Shared))`: the admission path of
/// `acquire_with_fixed_reservation` decided without admitting, for a demand of any size. A
/// demand the fixed reservation does not cover is refused by `try_admit_unqueued`, or queued by
/// `acquire_fair`, whenever a waiter is queued, so shared room alone does not admit it.
pub(in crate::swarm::transport) fn admits_now(
    queue_empty: bool,
    reserved: impl Fn(CapacityScope) -> bool,
) -> bool {
    reserved(CapacityScope::FixedReservation) || (queue_empty && reserved(CapacityScope::Shared))
}

pub(super) struct GlobalTransferCapacity {
    state: Mutex<ReservedCapacity<{ TransferClass::COUNT }>>,
    waiters: Arc<FairWaitQueue>,
    wait_budget: Arc<FairWaitBudget>,
    /// Advanced after every release of a global permit, which every transfer holds, so it
    /// counts every transfer's release of its peer and global capacity.
    releases: Epoch,
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
            releases: Epoch::default(),
        }
    }

    /// Register for every event after which the global part of `Room` may change: its
    /// releases and its queue's departures.
    pub(super) fn room_listeners(&self) -> [EventListener; 2] {
        [self.releases.listen(), self.waiters.departures().listen()]
    }

    /// The pure reservation step: `state` after admitting `bytes` of `class` in `scope`, or
    /// the refusal. Shared by `try_acquire_inner` (which commits it) and `has_room` (which does
    /// not), so the room predicate is the admission rule itself.
    fn reserved(
        mut state: ReservedCapacity<{ TransferClass::COUNT }>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
        scope: CapacityScope,
    ) -> Result<ReservedCapacity<{ TransferClass::COUNT }>> {
        let reservations = global_byte_reservations();
        let refused = || memory_capacity_error(peer, bytes, global_byte_limit(class));
        if scope == CapacityScope::FixedReservation
            && !state.reservation_covers(class.index(), bytes, reservations)
        {
            return Err(refused());
        }
        if !state.try_reserve(
            class.index(),
            bytes,
            OUTBOUND_GLOBAL_BYTE_CAPACITY,
            reservations,
        ) {
            return Err(refused());
        }
        Ok(state)
    }

    /// Whether `demand` could be admitted now by the global capacity (see `admits_now`).
    fn has_room(&self, peer: Did, demand: TransferDemand) -> bool {
        let state = *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        admits_now(self.waiters.is_empty(), |scope| {
            Self::reserved(state, peer, demand.class, demand.bytes, scope).is_ok()
        })
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
        let next = Self::reserved(*state, peer, class, bytes, scope)?;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::observe_outbound_global_capacity(
            next.admitted(),
            OUTBOUND_GLOBAL_BYTE_CAPACITY,
        );
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
            fixed_request_bytes(global_byte_reservations(), class),
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
            transfer_reservations(),
            peer_byte_reservations(),
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
            transfer_reservations(),
            OUTBOUND_PEER_BYTE_CAPACITY,
            peer_byte_reservations(),
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
    /// Advanced after every release of this peer's capacity, a half reservation included:
    /// the events after which `Room` may change.
    releases: Epoch,
    /// Advanced when a whole transfer of this peer ends (`TransferCapacityPermit` dropped):
    /// the progress of the peer's link. A half reservation held no frames and is not counted.
    progress: Epoch,
}

impl TransferCapacity {
    pub(super) fn new(global: Arc<GlobalTransferCapacity>) -> Self {
        let wait_budget = global.wait_budget.clone();
        Self {
            state: Mutex::new(PeerCapacityState::new()),
            global,
            waiters: Arc::new(FairWaitQueue::with_budget(wait_budget)),
            releases: Epoch::default(),
            progress: Epoch::default(),
        }
    }

    /// The pure reservation step of this peer (see `GlobalTransferCapacity::reserved`).
    fn reserved(
        mut state: PeerCapacityState,
        peer: Did,
        class: TransferClass,
        bytes: usize,
        scope: CapacityScope,
    ) -> Result<PeerCapacityState> {
        if scope == CapacityScope::FixedReservation && !state.reservation_covers(class, bytes) {
            return Err(memory_capacity_error(peer, bytes, peer_byte_limit(class)));
        }
        match state.try_reserve(class, bytes) {
            Ok(()) => Ok(state),
            Err(CountedReservationRejection::Count) => {
                Err(Error::OutboundTransferCapacityExceeded {
                    peer,
                    capacity: transfer_limit(class),
                })
            }
            Err(CountedReservationRejection::Bytes) => {
                Err(memory_capacity_error(peer, bytes, peer_byte_limit(class)))
            }
        }
    }

    /// `Room(peer, demand)`: `demand` could be admitted now by this peer and the global
    /// capacity (see `admits_now`), without admitting it.
    pub(super) fn has_room(&self, peer: Did, demand: TransferDemand) -> bool {
        let state = *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        admits_now(self.waiters.is_empty(), |scope| {
            Self::reserved(state, peer, demand.class, demand.bytes, scope).is_ok()
        }) && self.global.has_room(peer, demand)
    }

    /// `Room(peer, demand)` for a peer that holds no capacity: its own state is fresh and its
    /// queue empty, so only the fixed or shared step decides.
    pub(super) fn has_room_unheld(
        global: &GlobalTransferCapacity,
        peer: Did,
        demand: TransferDemand,
    ) -> bool {
        let fresh = PeerCapacityState::new();
        admits_now(true, |scope| {
            Self::reserved(fresh, peer, demand.class, demand.bytes, scope).is_ok()
        }) && global.has_room(peer, demand)
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
        let next = Self::reserved(*state, peer, class, bytes, scope)?;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::observe_outbound_peer_capacity(
            next.capacity.admitted_count(),
            OUTBOUND_TRANSFER_QUEUE_CAPACITY,
            next.capacity.admitted_bytes(),
            OUTBOUND_PEER_BYTE_CAPACITY,
        );
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
            fixed_request_bytes(peer_byte_reservations(), class),
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

    /// Register for every event after which `Room` of this peer may change: its releases and
    /// its queue's departures.
    pub(super) fn room_listeners(&self) -> [EventListener; 2] {
        [self.releases.listen(), self.waiters.departures().listen()]
    }

    /// Live permits of this peer: `0` iff no transfer of the peer holds frames in flight.
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

/// What one transfer asks of outbound capacity: its class and its reservation in bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) struct TransferDemand {
    /// The scheduling class the transfer is admitted under.
    class: TransferClass,
    /// The reservation in bytes (at least one).
    bytes: usize,
}

impl TransferDemand {
    /// The demand of a transfer of `class` reserving `bytes`, as `acquire` charges it.
    pub(in crate::swarm::transport) fn new(class: TransferClass, bytes: usize) -> Self {
        Self {
            class,
            bytes: bytes.max(1),
        }
    }

    /// A demand for tests that only carry one through the automaton.
    #[cfg(test)]
    pub(in crate::swarm::transport) fn for_test() -> Self {
        Self::new(TransferClass::Application, 1)
    }

    /// The class the demand is admitted under.
    pub(super) const fn class(self) -> TransferClass {
        self.class
    }

    /// The bytes the demand reserves.
    pub(super) const fn bytes(self) -> usize {
        self.bytes
    }
}

/// A reading of one peer's progress epoch, taken after a refused send published its refusal,
/// so after the refused send released whatever it held.
///
/// The capacity is held weakly: a dead capacity means every permit of the peer was released,
/// and a live `Weak` pins the allocation, so a recreated capacity is never mistaken for the
/// stamped one.
pub(in crate::swarm::transport) struct PeerStamp {
    /// The peer's capacity when stamped, if one existed.
    capacity: Weak<TransferCapacity>,
    /// Its progress count when stamped.
    released: u64,
}

/// What the peer's capacity shows against a [`PeerStamp`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(in crate::swarm::transport) struct PeerProgress {
    /// Another transfer of the peer ended since the stamp.
    pub(in crate::swarm::transport) released: bool,
    /// `Idle(peer)`: no transfer of the peer holds capacity.
    pub(in crate::swarm::transport) idle: bool,
}

impl PeerStamp {
    /// Stamp `capacity`, the peer's current capacity if any.
    pub(super) fn of(capacity: Option<&Arc<TransferCapacity>>) -> Self {
        Self {
            capacity: capacity.map_or_else(Weak::new, Arc::downgrade),
            released: capacity.map_or(0, |capacity| capacity.progress.current()),
        }
    }

    /// The peer's progress now against this stamp; a released capacity is idle.
    pub(in crate::swarm::transport) fn progress(&self) -> PeerProgress {
        self.capacity.upgrade().map_or(
            PeerProgress {
                released: true,
                idle: true,
            },
            |capacity| PeerProgress {
                released: capacity.progress.current() > self.released,
                idle: capacity.admitted() == 0,
            },
        )
    }

    /// Register for the peer's next progress, while its capacity lives.
    pub(in crate::swarm::transport) fn listen(&self) -> Option<EventListener> {
        self.capacity
            .upgrade()
            .map(|capacity| capacity.progress.listen())
    }
}

/// The peer and global capacity one transfer holds until it ends.
///
/// Fields drop in declaration order, so the global release, which advances
/// `GlobalTransferCapacity::releases`, follows the peer release.
pub(in crate::swarm::transport) struct TransferCapacityPermit {
    _peer: PeerCapacityPermit,
    _global: GlobalCapacityPermit,
}

impl Drop for TransferCapacityPermit {
    /// A whole transfer of the peer ended: its frames have left the link, which is progress.
    /// The fields release the peer and global capacity right after.
    fn drop(&mut self) {
        self._peer.capacity.progress.advance();
    }
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
        self.capacity.releases.advance();
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
        self.capacity.releases.advance();
    }
}

pub(super) fn transfer_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_TRANSFER_QUEUE_CAPACITY,
        transfer_reservations(),
        class.index(),
    )
}

const fn production_transfer_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_TRANSFER_QUEUE_CAPACITY,
        &OUTBOUND_TRANSFER_RESERVATIONS,
        class.index(),
    )
}

pub(super) fn peer_byte_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_PEER_BYTE_CAPACITY,
        peer_byte_reservations(),
        class.index(),
    )
}

const fn production_peer_byte_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_PEER_BYTE_CAPACITY,
        &OUTBOUND_PEER_BYTE_RESERVATIONS,
        class.index(),
    )
}

fn global_byte_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_GLOBAL_BYTE_CAPACITY,
        global_byte_reservations(),
        class.index(),
    )
}

const fn production_global_byte_limit(class: TransferClass) -> usize {
    admissible_capacity(
        OUTBOUND_GLOBAL_BYTE_CAPACITY,
        &OUTBOUND_GLOBAL_BYTE_RESERVATIONS,
        class.index(),
    )
}

const _: () = {
    let maximum_payload_reservation = retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE);
    assert_production_capacity(TransferClass::DhtControl, maximum_payload_reservation);
    assert_production_capacity(TransferClass::Storage, maximum_payload_reservation);
    assert_production_capacity(TransferClass::E2e, maximum_payload_reservation);
    assert_production_capacity(TransferClass::Application, maximum_payload_reservation);
};

const fn assert_production_capacity(class: TransferClass, maximum_payload_reservation: usize) {
    assert!(production_transfer_limit(class) > 0);
    assert!(maximum_payload_reservation <= production_peer_byte_limit(class));
    assert!(maximum_payload_reservation <= production_global_byte_limit(class));
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
                    admitted_by_class[class_index] =
                        admitted_by_class[class_index].saturating_sub(1);
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
