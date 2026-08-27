//! Fair, bounded admission with fixed per-class reservations.
//!
//! Reservation law: a class may borrow only the capacity left after preserving
//! every other class's unmet minimum. Fixed-reservation requests never wait
//! behind borrowers; larger requests share one FIFO queue with a hard retained-
//! memory budget.

mod capacity;
mod wait_queue;

pub(crate) use capacity::admissible_capacity;
pub(crate) use capacity::retained_wire_bytes;
pub(crate) use capacity::try_reserve_atomic;
pub(crate) use capacity::CountedReservationRejection;
pub(crate) use capacity::CountedReservedCapacity;
pub(crate) use capacity::ReservedCapacity;
pub(crate) use wait_queue::acquire_fair;
pub(crate) use wait_queue::FairWaitBudget;
pub(crate) use wait_queue::FairWaitQueue;

#[cfg(test)]
mod fair_wait_queue_tests;
#[cfg(test)]
mod reserved_capacity_tests;
