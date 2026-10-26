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
use crate::fair_admission::ResourceOrderedQueue;
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
/// covered arrival ([`InboundCharge::Covered`]) draws on nothing and waits for its stream alone.
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
}

/// What one inbound arrival reserves on the mailbox ledger (load-bounds paper, Law 4, #932).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InboundCharge {
    /// The retained bytes of the arrival: a whole frame's, or, for the first chunk of a
    /// message, the whole message's.
    Bytes(usize),
    /// Nothing: a later chunk of a message, whose first chunk was charged for all of it.
    Covered,
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

/// A set of at most three inbound resources.
pub(crate) type InboundResources =
    std::iter::Flatten<std::array::IntoIter<Option<InboundResource>, 3>>;

/// The inbound resources of `resources`.
fn inbound_resources(resources: [Option<InboundResource>; 3]) -> InboundResources {
    resources.into_iter().flatten()
}

pub(crate) struct InboundCapacity {
    state: Mutex<InboundCapacityState>,
    peer_states: Mutex<BTreeMap<Option<Did>, InboundPeerCapacityState>>,
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
        let held = match request.charge {
            InboundCharge::Bytes(bytes) => {
                self.reserve(request.peer, (request.lane, bytes))?;
                Some(HeldCharge {
                    capacity: Arc::clone(self),
                    peer: request.peer,
                    lane: request.lane,
                    bytes,
                })
            }
            InboundCharge::Covered => None,
        };
        Ok(InboundCapacityPermit {
            held,
            #[cfg(test)]
            _event: LiveEvent::new(self),
        })
    }

    /// Reserve `bytes` of `peer` on `lane`: both budgets change together, or neither does and
    /// the refusing budget is named.
    fn reserve(
        &self,
        peer: Option<Did>,
        (lane, bytes): (InboundLane, usize),
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
        if let InboundCharge::Bytes(bytes) = charge {
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
        let InboundCharge::Bytes(bytes) = request.charge else {
            return inbound_resources([None, None, None]);
        };
        let reserved = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .covers(request.lane, bytes);
        inbound_resources([
            Some(InboundResource::Peer(request.peer)),
            Some(InboundResource::Lane(request.lane)),
            (!reserved).then_some(InboundResource::Shared),
        ])
    }

    fn try_admit(
        &self,
        request: &InboundRequest,
    ) -> std::result::Result<InboundCapacityPermit, InboundResources> {
        self.try_reserve(request).map_err(|refusal| match refusal {
            InboundRefusal::Peer => {
                inbound_resources([Some(InboundResource::Peer(request.peer)), None, None])
            }
            InboundRefusal::Mailbox => inbound_resources([
                Some(InboundResource::Lane(request.lane)),
                Some(InboundResource::Shared),
                None,
            ]),
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
}

impl InboundRefusal {
    /// Test builds: the error of refusing `bytes` for `peer`.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn into_error(self, peer: Option<Did>, bytes: usize) -> Error {
        match self {
            Self::Peer => peer_memory_capacity_error(peer, bytes),
            Self::Mailbox => Error::InboundMailboxMemoryCapacityExceeded {
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
        drop((state, peer_states));
        self.capacity.applied.bump();
        self.capacity.waiters.serve(&self.capacity);
        // Test builds: an inbound release is a state change that quiescence probes read.
        #[cfg(test)]
        crate::tests::activity::record_activity();
    }
}

/// The inbound capacity one admitted event holds: its charge, if it has one, returned to its
/// ledger when dropped.
///
/// ```text
/// take ; restore(c) = id     restore(take) leaves the ledger unchanged
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

    /// Put `charge` back on this permit: the move of a completed message's charge onto the
    /// event that delivers it (#932), never a new charge.
    pub(super) fn restore(&mut self, charge: Option<HeldCharge>) {
        self.held = charge;
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
