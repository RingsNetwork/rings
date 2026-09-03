//! Bounded inbound capacity: request-size validation, node-wide and per-peer
//! reservation accounting, and the RAII permit that releases it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

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
use crate::fair_admission::CountedReservationRejection;
use crate::fair_admission::CountedReservedCapacity;
use crate::utils::GenerationWitness;

const _: () = {
    // One peer cannot consume the node budget, every lane retains a fixed
    // minimum, and one maximum legal frame always fits that minimum.
    assert!(INBOUND_PEER_CAPACITY < INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_PEER_BYTE_CAPACITY < INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE) <= INBOUND_PEER_BYTE_CAPACITY);
    assert!(INBOUND_RESERVED_TRANSFERS_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_RESERVED_BYTES_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(memory_reservation(MAX_DATA_CHANNEL_MESSAGE_SIZE) <= INBOUND_RESERVED_BYTES_PER_LANE);
};

pub(super) const fn memory_reservation(bytes: usize) -> usize {
    retained_wire_bytes(bytes)
}

pub(super) fn memory_capacity_error(requested_bytes: usize) -> Error {
    Error::InboundMailboxMemoryCapacityExceeded {
        requested_bytes,
        capacity_bytes: INBOUND_MAILBOX_BYTE_CAPACITY,
    }
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

pub(crate) struct InboundCapacity {
    state: Mutex<InboundCapacityState>,
    peer_states: Mutex<BTreeMap<Option<Did>, InboundPeerCapacityState>>,
    /// Bumped after every reservation, transition, or release is applied and
    /// its locks are released, so tests await the admitted count by event.
    applied: GenerationWitness,
}

impl InboundCapacity {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(InboundCapacityState::new()),
            peer_states: Mutex::new(BTreeMap::new()),
            applied: GenerationWitness::default(),
        }
    }

    pub(super) fn try_acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        let mut peer_states = self
            .peer_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next_peer = peer_states.get(&peer).copied().unwrap_or_default();
        match next_peer.try_reserve(bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::InboundPeerCapacityExceeded {
                    peer,
                    capacity: INBOUND_PEER_CAPACITY,
                });
            }
            Err(CountedReservationRejection::Bytes) => {
                return Err(peer_memory_capacity_error(peer, bytes));
            }
        }
        match state.try_reserve(lane, bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::InboundMailboxCapacityExceeded {
                    capacity: INBOUND_MAILBOX_CAPACITY,
                });
            }
            Err(CountedReservationRejection::Bytes) => return Err(memory_capacity_error(bytes)),
        }
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::observe_inbound_capacity(
            (
                next_peer.0.admitted_count(),
                next_peer.0.admitted_bytes(),
                INBOUND_PEER_CAPACITY,
                INBOUND_PEER_BYTE_CAPACITY,
            ),
            (
                state.0.admitted_count(),
                state.0.admitted_bytes(),
                INBOUND_MAILBOX_CAPACITY,
                INBOUND_MAILBOX_BYTE_CAPACITY,
            ),
        );
        peer_states.insert(peer, next_peer);
        drop((state, peer_states));
        self.applied.bump();
        Ok(InboundCapacityPermit {
            capacity: self.clone(),
            peer,
            lane,
            bytes,
        })
    }

    pub(super) fn acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        validate_memory_request(lane, bytes)?;
        validate_peer_memory_request(peer, bytes)?;
        self.try_acquire(peer, lane, bytes)
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn admitted_count_for_test(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .admitted_count()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_admitted_count_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.applied
            .await_until(|_generation| predicate(self.admitted_count_for_test()))
            .await;
    }
}

pub(super) struct InboundCapacityPermit {
    capacity: Arc<InboundCapacity>,
    peer: Option<Did>,
    pub(super) lane: InboundLane,
    pub(super) bytes: usize,
}

impl InboundCapacityPermit {
    pub(super) fn try_transition(&mut self, lane: InboundLane, bytes: usize) -> Result<()> {
        if lane == self.lane && bytes == self.bytes {
            return Ok(());
        }
        validate_memory_request(lane, bytes)?;
        validate_peer_memory_request(self.peer, bytes)?;
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
        let mut next_peer = peer_states.get(&self.peer).copied().unwrap_or_default();
        next_peer.release(self.bytes);
        match next_peer.try_reserve(bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::InboundPeerCapacityExceeded {
                    peer: self.peer,
                    capacity: INBOUND_PEER_CAPACITY,
                });
            }
            Err(CountedReservationRejection::Bytes) => {
                return Err(peer_memory_capacity_error(self.peer, bytes));
            }
        }
        let mut next = *state;
        next.release(self.lane, self.bytes);
        match next.try_reserve(lane, bytes) {
            Ok(()) => {
                peer_states.insert(self.peer, next_peer);
                *state = next;
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
                self.lane = lane;
                self.bytes = bytes;
                drop((state, peer_states));
                self.capacity.applied.bump();
                Ok(())
            }
            Err(CountedReservationRejection::Count) => Err(Error::InboundMailboxCapacityExceeded {
                capacity: INBOUND_MAILBOX_CAPACITY,
            }),
            Err(CountedReservationRejection::Bytes) => Err(memory_capacity_error(bytes)),
        }
    }
}

impl Drop for InboundCapacityPermit {
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
    }
}
