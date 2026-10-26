//! Bounded inbound capacity: request-size validation, node-wide and per-peer
//! reservation accounting, and the RAII permit that releases it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

use super::InboundLane;
use super::INBOUND_LANE_COUNT;
use super::INBOUND_MAILBOX_BYTE_CAPACITY;
use super::INBOUND_MAILBOX_CAPACITY;
use super::INBOUND_PEER_BYTE_CAPACITY;
use super::INBOUND_PEER_CAPACITY;
use super::INBOUND_RESERVED_BYTES;
use super::INBOUND_RESERVED_BYTES_PER_LANE;
use super::INBOUND_RESERVED_TRANSFERS;
use super::INBOUND_RESERVED_TRANSFERS_PER_LANE;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::admissible_capacity;
use crate::fair_admission::retained_wire_bytes;
use crate::fair_admission::AdmissionLedger;
use crate::fair_admission::CountedReservationRejection;
use crate::fair_admission::CountedReservedCapacity;
use crate::fair_admission::Principal;
use crate::fair_admission::ResourceOrderedQueue;
use crate::fair_admission::Shares;
use crate::fair_admission::SlotAccounting;
use crate::fair_admission::SlotFloorUse;
use crate::utils::GenerationWitness;

const _: () = {
    // One peer cannot consume the node budget, every lane retains a fixed
    // minimum, and one maximum legal frame always fits that minimum.
    assert!(INBOUND_PEER_CAPACITY < INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_PEER_BYTE_CAPACITY < INBOUND_MAILBOX_BYTE_CAPACITY);
    // The largest charge an honest first chunk takes, `n·d₀` for a message of at most
    // `TRANSPORT_MAX_SIZE` cut into data-channel-sized chunks, fits one peer's budget (#932).
    assert!(
        retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE + MAX_DATA_CHANNEL_MESSAGE_SIZE)
            <= INBOUND_PEER_BYTE_CAPACITY
    );
    assert!(INBOUND_RESERVED_TRANSFERS_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_RESERVED_BYTES_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(memory_reservation(MAX_DATA_CHANNEL_MESSAGE_SIZE) <= INBOUND_RESERVED_BYTES_PER_LANE);
    // The largest honest message fits the open-message pool alone, and the open messages of
    // every slot of the default topology fit the mailbox (load-bounds paper, Def. 6).
    assert!(
        retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE + MAX_DATA_CHANNEL_MESSAGE_SIZE)
            <= OPEN_SHARES.pool
    );
    assert!(
        OPEN_SHARES.bound(crate::dht::topology::edge_slots(
            crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY
        )) <= INBOUND_MAILBOX_BYTE_CAPACITY
    );
};

/// The shares of the open-message table `O` (load-bounds paper, Def. 6), in retained bytes:
/// every slot's floor `f_O` of 256 KiB, a message of up to 128 KiB, and the pool `P_O` of
/// 128 MiB, the largest message, shared in fair claims; no local or dedicated share.
///
/// ```text
/// B_O(s) = R(s)·f_O + P_O      B_O(3) = 328 · 256 KiB + 128 MiB = 210 MiB ≤ M = 256 MiB
/// ```
const OPEN_SHARES: Shares = Shares {
    floor: 256 * 1024,
    pool: 128 * 1024 * 1024,
    local: 0,
    inbox: 0,
    custody: 0,
};

pub(super) const fn memory_reservation(bytes: usize) -> usize {
    retained_wire_bytes(bytes)
}

pub(super) fn peer_memory_capacity_error(peer: Option<Did>, requested_bytes: usize) -> Error {
    Error::InboundPeerMemoryCapacityExceeded {
        peer,
        requested_bytes,
        capacity_bytes: INBOUND_PEER_BYTE_CAPACITY,
    }
}

pub(super) fn validate_peer_memory_request(
    peer: Option<Did>,
    requested_bytes: usize,
) -> Result<()> {
    if requested_bytes > INBOUND_PEER_BYTE_CAPACITY {
        return Err(peer_memory_capacity_error(peer, requested_bytes));
    }
    Ok(())
}

pub(super) fn validate_memory_request(lane: InboundLane, requested_bytes: usize) -> Result<()> {
    let limit = admissible_capacity(
        INBOUND_MAILBOX_BYTE_CAPACITY,
        &INBOUND_RESERVED_BYTES,
        lane.index(),
    );
    if requested_bytes > limit {
        return Err(Error::InboundMailboxMemoryCapacityExceeded {
            requested_bytes,
            capacity_bytes: limit,
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct InboundCapacityState(CountedReservedCapacity<INBOUND_LANE_COUNT>);

impl InboundCapacityState {
    const fn new() -> Self {
        Self(CountedReservedCapacity::new())
    }
    fn try_reserve(
        &mut self,
        lane: InboundLane,
        bytes: usize,
    ) -> std::result::Result<(), CountedReservationRejection> {
        CountedReservedCapacity::try_reserve(
            &mut self.0,
            lane.index(),
            bytes,
            INBOUND_MAILBOX_CAPACITY,
            &INBOUND_RESERVED_TRANSFERS,
            INBOUND_MAILBOX_BYTE_CAPACITY,
            &INBOUND_RESERVED_BYTES,
        )
    }
    fn release(&mut self, lane: InboundLane, bytes: usize) {
        self.0.release(lane.index(), bytes);
    }

    /// Whether `lane`'s fixed reservation alone covers one more arrival of `bytes`.
    fn covers(&self, lane: InboundLane, bytes: usize) -> bool {
        self.0.reservation_covers(
            lane.index(),
            bytes,
            &INBOUND_RESERVED_TRANSFERS,
            &INBOUND_RESERVED_BYTES,
        )
    }
}

const PEER_RESERVATION: [usize; 1] = [0];

#[derive(Clone, Copy, Default)]
struct InboundPeerCapacityState(CountedReservedCapacity<1>);

impl InboundPeerCapacityState {
    fn try_reserve(
        &mut self,
        bytes: usize,
    ) -> std::result::Result<(), CountedReservationRejection> {
        CountedReservedCapacity::try_reserve(
            &mut self.0,
            0,
            bytes,
            INBOUND_PEER_CAPACITY,
            &PEER_RESERVATION,
            INBOUND_PEER_BYTE_CAPACITY,
            &PEER_RESERVATION,
        )
    }

    fn release(&mut self, bytes: usize) {
        self.0.release(0, bytes);
    }

    const fn is_idle(self) -> bool {
        self.0.admitted_count() == 0
    }
}

/// A resource an inbound arrival draws on, or the stream it is ordered in.
///
/// A charged arrival of `peer` on `lane` draws on `Peer(peer)` and `Lane(lane)`, and on `Shared`
/// when its lane's fixed reservation does not cover it. A refusal by the peer's budget names
/// `Peer(peer)`; a refusal by the node-wide budget names `Lane(lane)` and `Shared`, since a
/// lane is refused only beyond its reservation. So a frame that fits its lane's reservation never
/// waits behind a borrower of the shared pool, and no peer waits behind another peer's budget. A
/// first chunk also draws on `Floor(introducer)`, and on `OpenPool` beyond that floor.
///
/// A footprint depends on the ledger: `Lane(lane)` is the relief of `Shared`, whose release
/// may let the reservation cover the arrival, and `Floor(introducer)` that of `OpenPool`. An
/// arrival blocked only for lack of room there holds its reliefs, so a reservation and a floor
/// are each served first come first served: a later, smaller frame does not pass an earlier one
/// on them. An arrival blocked by its stream or its own peer's budget holds nothing. A covered
/// arrival ([`InboundCharge::Covered`]) draws on nothing and waits for its stream alone.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum InboundResource {
    /// The arrivals of one peer on one lane through one transport carrier, admitted in arrival
    /// order: the order the sender's class lane gave them, in which a chunked message's chunks
    /// are contiguous.
    Stream(Option<Did>, InboundLane, Option<ChannelLane>),
    /// One peer's budget.
    Peer(Option<Did>),
    /// One lane's share of the node-wide budget.
    Lane(InboundLane),
    /// The node-wide budget beyond the lanes' fixed reservations.
    Shared,
    /// The pool of the open-message table, which a first chunk beyond its slot's floor draws
    /// on in fair claims; a first chunk within its floor never waits for it.
    OpenPool,
    /// One introducer's floor in the open-message table, which every first chunk it introduces
    /// draws on; its release may bring a waiting first chunk within the floor.
    Floor(Principal),
}

/// What one inbound arrival reserves on the mailbox ledger (load-bounds paper, Law 4, #932).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InboundCharge {
    /// The retained bytes of a whole frame, on its own lane.
    Bytes(usize),
    /// The first chunk of a message: the whole message's retained bytes, on the lane of its
    /// carrier's class, which dispatches it once complete, and in the open-message table,
    /// charged to the slot that introduced it until the message completes or expires.
    Message {
        /// The message's retained bytes, `r(n·d₀)`.
        bytes: usize,
        /// The lane of its carrier's class.
        lane: InboundLane,
        /// The slot of the edge that delivered the first chunk.
        introducer: Principal,
    },
    /// Nothing: a later chunk of a message, whose first chunk was charged for all of it.
    Covered,
}

impl InboundCharge {
    /// The lane and bytes this charge reserves on the mailbox ledger, for an arrival of `lane`;
    /// `None` for a covered arrival.
    const fn reservation(self, lane: InboundLane) -> Option<(InboundLane, usize)> {
        match self {
            Self::Bytes(bytes) => Some((lane, bytes)),
            Self::Message { bytes, lane, .. } => Some((lane, bytes)),
            Self::Covered => None,
        }
    }

    /// The principal and bytes this charge holds in the open-message table, if it opens a
    /// message.
    const fn opening(self) -> Option<(Principal, usize)> {
        match self {
            Self::Message {
                bytes, introducer, ..
            } => Some((introducer, bytes)),
            Self::Bytes(_) | Self::Covered => None,
        }
    }
}

/// One inbound arrival's capacity request.
pub(crate) struct InboundRequest {
    /// The sending peer, if known.
    peer: Option<Did>,
    /// The lane the arrival is classified to.
    lane: InboundLane,
    /// The transport lane whose credit the arrival took, if any.
    carrier: Option<ChannelLane>,
    /// What the arrival reserves.
    charge: InboundCharge,
}

/// A set of at most four inbound resources.
pub(crate) type InboundResources =
    std::iter::Flatten<std::array::IntoIter<Option<InboundResource>, 5>>;

/// The inbound resources of `resources`.
fn inbound_resources(resources: [Option<InboundResource>; 5]) -> InboundResources {
    resources.into_iter().flatten()
}

pub(crate) struct InboundCapacity {
    state: Mutex<InboundCapacityState>,
    peer_states: Mutex<BTreeMap<Option<Did>, InboundPeerCapacityState>>,
    /// The open-message table `O`: the bytes open messages hold, by the slot that introduced
    /// them (load-bounds paper, Def. 6). Its lock is a leaf, taken after the others.
    open: Mutex<SlotAccounting>,
    /// Arrivals waiting for capacity ([`InboundResource`]); the credit law bounds them, per
    /// connection, by its lanes' windows, since each holds a frame admitted under credit.
    waiters: ResourceOrderedQueue<Arc<InboundCapacity>>,
    /// Bumped after every reservation or release is applied and its locks are released, and
    /// after every event ends, so tests await the admitted count by event.
    applied: GenerationWitness,
    /// Test builds: the admitted events not yet handled ([`LiveEvent`]).
    #[cfg(test)]
    live_events: std::sync::atomic::AtomicUsize,
}

impl InboundCapacity {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(InboundCapacityState::new()),
            peer_states: Mutex::new(BTreeMap::new()),
            open: Mutex::new(SlotAccounting::new(OPEN_SHARES)),
            waiters: ResourceOrderedQueue::new(),
            applied: GenerationWitness::default(),
            #[cfg(test)]
            live_events: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Reserve `bytes` for `peer` on `lane` at once, or name the budget that refused it.
    #[cfg(test)]
    pub(super) fn try_acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> std::result::Result<InboundCapacityPermit, InboundRefusal> {
        self.try_reserve(&InboundRequest {
            peer,
            lane,
            carrier: None,
            charge: InboundCharge::Bytes(bytes),
        })
    }

    /// Reserve `request` at once, or name the budget that refused it; a covered request
    /// reserves nothing and is never refused.
    fn try_reserve(
        self: &Arc<Self>,
        request: &InboundRequest,
    ) -> std::result::Result<InboundCapacityPermit, InboundRefusal> {
        let held = match request.charge.reservation(request.lane) {
            Some((lane, bytes)) => {
                let open = request.charge.opening().map(|(introducer, _)| introducer);
                self.reserve(request.peer, (lane, bytes), open)?;
                Some(HeldCharge {
                    capacity: Arc::clone(self),
                    peer: request.peer,
                    lane,
                    bytes,
                    open,
                })
            }
            None => None,
        };
        Ok(InboundCapacityPermit {
            held,
            #[cfg(test)]
            _event: LiveEvent::new(self),
        })
    }

    /// Reserve `bytes` of `peer` on `lane`, and, for a message `opened` by a slot, the same bytes
    /// in the open-message table: every budget changes together, or none does and the refusing
    /// one is named.
    fn reserve(
        &self,
        peer: Option<Did>,
        (lane, bytes): (InboundLane, usize),
        opened: Option<Principal>,
    ) -> std::result::Result<(), InboundRefusal> {
        let mut peer_states = self
            .peer_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next_peer = peer_states.get(&peer).copied().unwrap_or_default();
        let mut next = *state;
        next_peer
            .try_reserve(bytes)
            .map_err(|_| InboundRefusal::Peer)?;
        next.try_reserve(lane, bytes)
            .map_err(|_| InboundRefusal::Mailbox)?;
        let mut open = self.open();
        if let Some(introducer) = opened {
            open.admit_items(introducer, bytes)
                .map_err(|_| InboundRefusal::Open)?;
            open.hold(introducer, bytes);
        }
        drop(open);
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::observe_inbound_capacity(
            (
                next_peer.0.admitted_count(),
                next_peer.0.admitted_bytes(),
                INBOUND_PEER_CAPACITY,
                INBOUND_PEER_BYTE_CAPACITY,
            ),
            (
                next.0.admitted_count(),
                next.0.admitted_bytes(),
                INBOUND_MAILBOX_CAPACITY,
                INBOUND_MAILBOX_BYTE_CAPACITY,
            ),
        );
        peer_states.insert(peer, next_peer);
        *state = next;
        drop((state, peer_states));
        self.applied.bump();
        Ok(())
    }

    /// Admit one arrival, waiting for capacity rather than refusing it.
    ///
    /// Law (no refusal of a valid arrival). An arrival within the request bounds is never
    /// dropped for want of mailbox capacity: it waits, in resource order ([`InboundResource`]),
    /// keeping its sender's lane credit, and the wait is logged at `warn` once. A request beyond
    /// the bounds is refused at once. An admitted charge is never re-charged: a chunked message
    /// is charged once, at its first chunk, and its later chunks are covered (#932).
    pub(super) async fn acquire(
        self: &Arc<Self>,
        (peer, lane, carrier): (Option<Did>, InboundLane, Option<ChannelLane>),
        charge: InboundCharge,
    ) -> Result<InboundCapacityPermit> {
        if let Some((lane, bytes)) = charge.reservation(lane) {
            validate_memory_request(lane, bytes)?;
            validate_peer_memory_request(peer, bytes)?;
        }
        let request = InboundRequest {
            peer,
            lane,
            carrier,
            charge,
        };
        Ok(self
            .waiters
            .acquire(self, request, || {
                // A wait is a state change that tests await by event.
                self.applied.bump();
                tracing::warn!(
                    peer = ?peer,
                    lane = ?lane,
                    charge = ?charge,
                    "inbound backpressure: the mailbox is full; the arrival waits for capacity \
                     and holds its sender's credit"
                );
            })
            .await)
    }

    /// The floor use of every slot in the open-message table at `now_ms` (load-bounds paper,
    /// Def. 1).
    pub(crate) fn open_floor_use_at(&self, now_ms: u128) -> SlotFloorUse {
        self.open().floor_use_at(now_ms)
    }

    /// The open-message table, for one pure step.
    fn open(&self) -> std::sync::MutexGuard<'_, SlotAccounting> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Test builds: the bytes open messages hold for `principal`.
    #[cfg(test)]
    pub(crate) fn open_bytes_for_test(&self, principal: Principal) -> usize {
        self.open().held(principal)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn admitted_count_for_test(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .admitted_count()
    }

    /// Test builds: the admitted events not yet handled; unlike the admitted count, it leaves
    /// out the charges open messages hold, which wait for their peer, not for this node.
    #[cfg(test)]
    pub(crate) fn live_events_for_test(&self) -> usize {
        self.live_events.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_admitted_count_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.applied
            .await_until(|_generation| predicate(self.admitted_count_for_test()))
            .await;
    }

    /// Resolve once `predicate` holds over the number of arrivals waiting for capacity,
    /// re-checked whenever an arrival starts to wait or capacity changes.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_waiting_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.applied
            .await_until(|_generation| predicate(self.waiters.waiting_for_test()))
            .await;
    }
}

/// The inbound budgets as the ledger of their waiting arrivals.
impl AdmissionLedger for Arc<InboundCapacity> {
    type Grant = InboundCapacityPermit;
    type Request = InboundRequest;
    type Resource = InboundResource;
    type Resources = InboundResources;

    fn stream(&self, request: &InboundRequest) -> InboundResource {
        InboundResource::Stream(request.peer, request.lane, request.carrier)
    }

    fn footprint(&self, request: &InboundRequest) -> InboundResources {
        let Some((lane, bytes)) = request.charge.reservation(request.lane) else {
            return inbound_resources([None, None, None, None, None]);
        };
        let reserved = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .covers(lane, bytes);
        let opening = request.charge.opening();
        // Within its slot's floor a first chunk is never refused by `O` (the floor guarantee).
        let pooled =
            opening.is_some_and(|(introducer, bytes)| !self.open().within_floor(introducer, bytes));
        inbound_resources([
            Some(InboundResource::Peer(request.peer)),
            Some(InboundResource::Lane(lane)),
            (!reserved).then_some(InboundResource::Shared),
            pooled.then_some(InboundResource::OpenPool),
            opening.map(|(introducer, _)| InboundResource::Floor(introducer)),
        ])
    }

    fn reliefs(&self, request: &InboundRequest) -> Vec<(InboundResource, Vec<InboundResource>)> {
        let Some((lane, bytes)) = request.charge.reservation(request.lane) else {
            return Vec::new();
        };
        // Every charge fits an idle reservation (the constant assertions above), so a lane
        // whose reservation does not cover it now may.
        let reserved = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .covers(lane, bytes);
        let floor = request.charge.opening().filter(|(introducer, bytes)| {
            let open = self.open();
            !open.within_floor(*introducer, *bytes) && open.fits_floor(*introducer, *bytes)
        });
        [
            (!reserved).then(|| (InboundResource::Lane(lane), vec![InboundResource::Shared])),
            floor.map(|(introducer, _)| {
                (InboundResource::Floor(introducer), vec![
                    InboundResource::OpenPool,
                ])
            }),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn try_admit(
        &self,
        request: &InboundRequest,
    ) -> std::result::Result<InboundCapacityPermit, InboundResources> {
        self.try_reserve(request).map_err(|refusal| match refusal {
            InboundRefusal::Peer => inbound_resources([
                Some(InboundResource::Peer(request.peer)),
                None,
                None,
                None,
                None,
            ]),
            InboundRefusal::Mailbox => inbound_resources([
                request
                    .charge
                    .reservation(request.lane)
                    .map(|(lane, _)| InboundResource::Lane(lane)),
                Some(InboundResource::Shared),
                None,
                None,
                None,
            ]),
            InboundRefusal::Open => {
                inbound_resources([Some(InboundResource::OpenPool), None, None, None, None])
            }
        })
    }
}

/// The budget that refused a reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InboundRefusal {
    /// The peer's budget.
    Peer,
    /// The node-wide budget.
    Mailbox,
    /// The open-message table: its pool, or the claimant's fair claim of it.
    Open,
}

impl InboundRefusal {
    /// Test builds: the error of refusing `bytes` for `peer`.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn into_error(self, peer: Option<Did>, bytes: usize) -> Error {
        match self {
            Self::Peer => peer_memory_capacity_error(peer, bytes),
            Self::Mailbox | Self::Open => Error::InboundMailboxMemoryCapacityExceeded {
                requested_bytes: bytes,
                capacity_bytes: INBOUND_MAILBOX_BYTE_CAPACITY,
            },
        }
    }
}

/// Capacity reserved on the mailbox ledger for one peer on one lane, returned to the ledger
/// when dropped: the charge of one frame, or of one chunked message (#932).
pub(crate) struct HeldCharge {
    /// The ledger the capacity is held in.
    capacity: Arc<InboundCapacity>,
    /// The sending peer, if known, whose share the charge draws on.
    peer: Option<Did>,
    /// The lane the charge is reserved on.
    lane: InboundLane,
    /// The retained bytes reserved.
    bytes: usize,
    /// The slot the bytes are held for in the open-message table, while the message they
    /// reserve is open.
    open: Option<Principal>,
}

impl HeldCharge {
    /// The message this charge reserves completed: its bytes leave the open-message table, and
    /// its mailbox reservation stays, moving with the message into dispatch (Law 4).
    fn close(&mut self) {
        if let Some(introducer) = self.open.take() {
            self.capacity.open().release(introducer, self.bytes);
            self.capacity.applied.bump();
            self.capacity.waiters.release(&self.capacity, [
                InboundResource::OpenPool,
                InboundResource::Floor(introducer),
            ]);
        }
    }

    /// The resources this charge frees when it is released: its peer's budget, its lane's
    /// share and the node-wide budget beyond the reservations, which a lane's release also
    /// frees for the other lanes, and the open-message pool and its introducer's floor while it
    /// holds a share of them.
    fn freed(&self) -> impl Iterator<Item = InboundResource> {
        [
            Some(InboundResource::Peer(self.peer)),
            Some(InboundResource::Lane(self.lane)),
            Some(InboundResource::Shared),
            self.open.map(|_| InboundResource::OpenPool),
            self.open.map(InboundResource::Floor),
        ]
        .into_iter()
        .flatten()
    }
}

impl Drop for HeldCharge {
    fn drop(&mut self) {
        let mut peer_states = self
            .capacity
            .peer_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.release(self.lane, self.bytes);
        if let Some(peer_state) = peer_states.get_mut(&self.peer) {
            peer_state.release(self.bytes);
            if peer_state.is_idle() {
                peer_states.remove(&self.peer);
            }
        }
        if let Some(introducer) = self.open {
            self.capacity.open().release(introducer, self.bytes);
        }
        drop((state, peer_states));
        self.capacity.applied.bump();
        self.capacity.waiters.release(&self.capacity, self.freed());
        // Test builds: an inbound release is a state change that quiescence probes read.
        #[cfg(test)]
        crate::tests::activity::record_activity();
    }
}

/// The inbound capacity one admitted event holds: its charge, if it has one, returned to its
/// ledger when dropped.
///
/// ```text
/// take ; restore = close     the mailbox ledger is unchanged; the message leaves `O`
/// ```
pub(crate) struct InboundCapacityPermit {
    /// The charge the event holds; `None` for a covered arrival, or once taken.
    held: Option<HeldCharge>,
    /// Test builds: the event's place in the live-event count, which quiescence probes read.
    #[cfg(test)]
    _event: LiveEvent,
}

impl InboundCapacityPermit {
    /// Take this permit's charge out, leaving a permit that holds nothing in its place: the move
    /// of a first chunk's charge into its message (#932), never a release or a new charge.
    pub(super) fn take(&mut self) -> Option<HeldCharge> {
        self.held.take()
    }

    /// Put a completed message's `charge` back on this permit: the message leaves the
    /// open-message table, and its mailbox reservation moves onto the event that delivers it
    /// (#932), never a new charge.
    pub(super) fn restore(&mut self, charge: Option<HeldCharge>) {
        self.held = charge.map(|mut charge| {
            charge.close();
            charge
        });
    }
}

/// Test builds: one admitted event, counted live on its ledger from its admission until it is
/// handled, whatever charge it holds; the charge an open message holds is not an event.
#[cfg(test)]
struct LiveEvent(Arc<InboundCapacity>);

#[cfg(test)]
impl LiveEvent {
    /// Count one event live on `capacity`.
    fn new(capacity: &Arc<InboundCapacity>) -> Self {
        capacity
            .live_events
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(Arc::clone(capacity))
    }
}

#[cfg(test)]
impl Drop for LiveEvent {
    fn drop(&mut self) {
        self.0
            .live_events
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.0.applied.bump();
        crate::tests::activity::record_activity();
    }
}
