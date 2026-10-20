use std::sync::Arc;
use std::sync::Mutex;

use event_listener::Event;
use event_listener::EventListener;
#[cfg(all(test, not(feature = "dummy"), not(target_family = "wasm")))]
use tokio::sync::watch;

use super::model::of_class;
use super::model::TransferClass;
use super::sender_bounded::SenderBoundedTransfer;
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

mod view;

pub(in crate::swarm::transport) use view::CapacityView;
pub(in crate::swarm::transport) use view::PeerProgress;
pub(in crate::swarm::transport) use view::PeerStamp;

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
    *of_class(reservations, class)
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

/// The no-wait form of [`acquire_with_fixed_reservation`]: the class's reserved share, else the
/// shared capacity only while no waiter is queued, so a request that cannot wait never overtakes
/// one that does.
fn admit_with_fixed_reservation<T>(
    waiters: &FairWaitQueue,
    capacity_error: Error,
    try_reserved: impl FnOnce() -> Result<T>,
    try_shared: impl FnOnce() -> Result<T>,
) -> Result<T> {
    try_reserved().or_else(|_| waiters.try_admit_unqueued(capacity_error, try_shared))
}

/// The class's reserved share, else, for a request larger than the class's fixed request, a
/// first-come-first-served wait on `waiters` for the shared capacity; a smaller request does not
/// wait ([`admit_with_fixed_reservation`]).
///
/// Law (FIFO, up to one class reservation). Every waiter of one queue waits for the shared
/// capacity of one scope (a peer's queue for that peer's, the global queue for the node's
/// shared bytes), since a request takes its reserved share before it queues and queues only
/// for the shared part. The part a class may borrow excludes the *other* classes' unused
/// reservations, so the waiters of one queue do not see quite the same resource: a head of one
/// class that cannot fit may hold back a waiter of another class that would, by at most one
/// class reservation. Within that bound the queue is the resource-ordered queue of
/// `fair_admission::resource_order` (which serves the inbound actor, whose waiters draw on
/// wholly different resources: lane, peer and node), and no request overtakes one refused by
/// its own class's share. Only application and storage-maintenance sends wait here; a protocol
/// send never does.
async fn acquire_with_fixed_reservation<T>(
    waiters: &Arc<FairWaitQueue>,
    bytes: usize,
    fixed_request_limit: usize,
    capacity_error: impl Fn() -> Error,
    try_reserved: impl FnOnce() -> Result<T>,
    mut try_shared: impl FnMut() -> Result<T>,
) -> Result<T> {
    if bytes <= fixed_request_limit {
        return admit_with_fixed_reservation(waiters, capacity_error(), try_reserved, try_shared);
    }
    if let Ok(permit) = try_reserved() {
        return Ok(permit);
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
    fn room_listeners(&self) -> [EventListener; 2] {
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

    /// [`Self::acquire`] without waiting: refused at once where it would queue.
    fn admit_now(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<GlobalCapacityPermit> {
        admit_with_fixed_reservation(
            &self.waiters,
            memory_capacity_error(peer, bytes, global_byte_limit(class)),
            || self.try_acquire_inner(peer, class, bytes, CapacityScope::FixedReservation),
            || self.try_acquire_inner(peer, class, bytes, CapacityScope::Shared),
        )
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
    /// Transfers of the peer that reached its link and ended: the progress of the link.
    ///
    /// Inv: incremented in the critical section that releases the transfer's peer capacity,
    /// so one reading of the state sees a transfer either admitted or counted here, never
    /// both and never neither.
    drained: u64,
}

impl PeerCapacityState {
    const fn new() -> Self {
        Self {
            capacity: CountedReservedCapacity::new(),
            drained: 0,
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
    /// Notified after `PeerCapacityState::drained` grows
    /// ([`TransferCapacityPermit::release_after_progress`]). A transfer that never admitted a
    /// frame to the link, and a half reservation, do not notify it.
    progressed: Event,
    /// Test witness of this capacity's deallocation. Nothing is ever sent on it, so its
    /// subscribers observe exactly one event, the channel closing when the capacity is
    /// dropped, and that event is terminal.
    #[cfg(all(test, not(feature = "dummy"), not(target_family = "wasm")))]
    retirement: watch::Sender<()>,
}

impl TransferCapacity {
    pub(super) fn new(global: Arc<GlobalTransferCapacity>) -> Self {
        let wait_budget = global.wait_budget.clone();
        Self {
            state: Mutex::new(PeerCapacityState::new()),
            global,
            waiters: Arc::new(FairWaitQueue::with_budget(wait_budget)),
            releases: Epoch::default(),
            progressed: Event::new(),
            #[cfg(all(test, not(feature = "dummy"), not(target_family = "wasm")))]
            retirement: watch::channel(()).0,
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
    fn has_room(&self, peer: Did, demand: TransferDemand) -> bool {
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
    fn has_room_unheld(global: &GlobalTransferCapacity, peer: Did, demand: TransferDemand) -> bool {
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
            reach: LinkReach::Unreached,
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
        Ok(TransferCapacityPermit::held(peer_permit, global_permit))
    }

    /// [`Self::acquire`] without waiting: a permit if both the peer's and the node's capacity
    /// admit `bytes` now, otherwise the refusal a wait would have started from.
    ///
    /// Law: it never overtakes a queued waiter, so a request that cannot wait (a protocol send,
    /// which runs inside the inbound event that carried it) costs the fair waiters nothing.
    /// The price is the converse: while waiters are queued on the shared pool, a request larger
    /// than its class's fixed reservation is refused, however long they wait, and a protocol send
    /// that size is dropped as local backpressure until the queue drains.
    pub(super) fn admit_now(
        self: &Arc<Self>,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        validate_memory_request(peer, class, bytes)?;
        let bytes = bytes.max(1);
        let peer_permit = admit_with_fixed_reservation(
            &self.waiters,
            memory_capacity_error(peer, bytes, peer_byte_limit(class)),
            || self.try_acquire_peer_inner(peer, class, bytes, CapacityScope::FixedReservation),
            || self.try_acquire_peer_inner(peer, class, bytes, CapacityScope::Shared),
        )?;
        let global_permit = self.global.admit_now(peer, class, bytes)?;
        Ok(TransferCapacityPermit::held(peer_permit, global_permit))
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
        Ok(TransferCapacityPermit::held(peer_permit, global_permit))
    }

    /// Register for every event after which `Room` of this peer may change: its releases and
    /// its queue's departures.
    fn room_listeners(&self) -> [EventListener; 2] {
        [self.releases.listen(), self.waiters.departures().listen()]
    }

    /// Live permits of this peer: `0` iff no transfer of the peer holds frames in flight.
    #[cfg(test)]
    pub(super) fn admitted(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capacity
            .admitted_count()
    }

    /// Subscribe to this capacity's deallocation; see `retirement`.
    #[cfg(all(test, not(feature = "dummy"), not(target_family = "wasm")))]
    pub(super) fn subscribe_retirement(&self) -> watch::Receiver<()> {
        self.retirement.subscribe()
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

/// The outbound capacity one transfer holds until it ends, released when it drops.
pub(in crate::swarm::transport) struct TransferCapacityPermit {
    _share: PermitShare,
}

/// What a [`TransferCapacityPermit`] holds.
enum PermitShare {
    /// A share of its peer's capacity and of the node's. Fields drop in declaration order, so
    /// the global release, which advances `GlobalTransferCapacity::releases`, follows the peer
    /// release (what a rerouted placement waiting for room reads, #859).
    Held {
        _peer: PeerCapacityPermit,
        _global: GlobalCapacityPermit,
    },
    /// No shared capacity: a transfer its sender's own law bounds (see
    /// `outbound::sender_bounded`).
    BoundedBySender { _transfer: SenderBoundedTransfer },
}

impl TransferCapacityPermit {
    /// A permit holding the peer's and the node's shares.
    fn held(peer: PeerCapacityPermit, global: GlobalCapacityPermit) -> Self {
        Self {
            _share: PermitShare::Held {
                _peer: peer,
                _global: global,
            },
        }
    }

    /// A permit holding no shared capacity, for a transfer its sender's own law bounds.
    pub(in crate::swarm::transport) fn bounded_by_sender(transfer: SenderBoundedTransfer) -> Self {
        Self {
            _share: PermitShare::BoundedBySender {
                _transfer: transfer,
            },
        }
    }
}

impl TransferCapacityPermit {
    /// Release the permit of a transfer that admitted a frame to the peer's link and has
    /// ended, counting it in `PeerCapacityState::drained`, then notify the peer's progress.
    ///
    /// Post: a waiter woken by this progress reads the peer and global capacity already
    /// released, so its retry is not refused by the transfer that woke it. Dropping a permit
    /// releases it without progress: nothing it reserved reached the link.
    /// A transfer bounded by its sender holds no peer capacity, so it counts in no drain.
    pub(in crate::swarm::transport) fn release_after_progress(mut self) {
        let capacity = match &mut self._share {
            PermitShare::Held { _peer, .. } => {
                _peer.reach = LinkReach::Reached;
                Some(Arc::clone(&_peer.capacity))
            }
            PermitShare::BoundedBySender { .. } => None,
        };
        drop(self);
        if let Some(capacity) = capacity {
            capacity.progressed.notify(usize::MAX);
        }
    }
}

/// Whether a transfer holding a peer permit admitted a frame to the peer's link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkReach {
    /// No frame of the transfer reached the link (a half reservation, or a transfer that
    /// ended before its first frame).
    Unreached,
    /// The transfer admitted a frame to the link and has ended.
    Reached,
}

struct PeerCapacityPermit {
    capacity: Arc<TransferCapacity>,
    class: TransferClass,
    bytes: usize,
    /// Whether the release counts as the link's progress.
    reach: LinkReach,
}

impl Drop for PeerCapacityPermit {
    /// Release the peer capacity and, for a transfer that reached the link, count it drained
    /// in the same critical section (Inv of `PeerCapacityState::drained`).
    fn drop(&mut self) {
        {
            let mut state = self
                .capacity
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.release(self.class, self.bytes);
            if self.reach == LinkReach::Reached {
                state.drained = state.drained.saturating_add(1);
            }
        }
        self.capacity.waiters.wake_front();
        self.capacity.releases.advance();
        // Test builds: an outbound release is a state change that quiescence probes read.
        #[cfg(test)]
        crate::tests::activity::record_activity();
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

/// A strong hold on a peer's [`TransferCapacity`]: the peer registry keeps only a `Weak`, so
/// the capacity lives as long as some handle or permit anchors it.
pub(super) struct TransferCapacityAnchor {
    _capacity: Arc<TransferCapacity>,
}

impl TransferCapacityAnchor {
    pub(super) fn new(capacity: Arc<TransferCapacity>) -> Self {
        Self {
            _capacity: capacity,
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn try_acquire(
        &self,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        self._capacity.try_acquire(peer, class, bytes)
    }
}

#[cfg(test)]
mod test_capacity;
