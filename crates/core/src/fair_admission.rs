//! Fair, bounded admission with fixed per-class reservations.
//!
//! Reservation law: a class may borrow only the capacity left after preserving
//! every other class's unmet minimum. Fixed-reservation requests never wait
//! behind borrowers; larger requests share one FIFO queue with a hard retained-
//! memory budget.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

pub(crate) fn fair_reservation_fits<const N: usize>(
    admitted_by_class: &[usize; N],
    admitted: usize,
    class_index: usize,
    amount: usize,
    capacity: usize,
    reservations: &[usize; N],
) -> bool {
    if class_index >= N {
        return false;
    }
    let reserved_for_others = admitted_by_class
        .iter()
        .zip(reservations)
        .enumerate()
        .filter(|(index, _)| *index != class_index)
        .map(|(_, (admitted, reserved))| reserved.saturating_sub(*admitted))
        .sum::<usize>();
    admitted
        .checked_add(amount)
        .is_some_and(|next| next <= capacity.saturating_sub(reserved_for_others))
}

const fn reserved_for_other_classes(reservations: &[usize], class_index: usize) -> usize {
    let Some((first, remaining)) = reservations.split_first() else {
        return 0;
    };
    if class_index == 0 {
        return reservation_sum(remaining);
    }
    first.saturating_add(reserved_for_other_classes(remaining, class_index - 1))
}

const fn reservation_sum(reservations: &[usize]) -> usize {
    let Some((first, remaining)) = reservations.split_first() else {
        return 0;
    };
    first.saturating_add(reservation_sum(remaining))
}

pub(crate) const fn admissible_capacity<const N: usize>(
    capacity: usize,
    reservations: &[usize; N],
    class_index: usize,
) -> usize {
    capacity.saturating_sub(reserved_for_other_classes(reservations, class_index))
}

pub(crate) const fn retained_wire_bytes(wire_bytes: usize) -> usize {
    wire_bytes.saturating_mul(2)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CountedReservationRejection {
    Count,
    Bytes,
}

pub(crate) fn fixed_reservation_covers<const N: usize>(
    admitted_by_class: &[usize; N],
    class_index: usize,
    amount: usize,
    reservations: &[usize; N],
) -> bool {
    admitted_by_class
        .get(class_index)
        .zip(reservations.get(class_index))
        .and_then(|(admitted, reserved)| admitted.checked_add(amount).map(|next| (next, reserved)))
        .is_some_and(|(next, reserved)| next <= *reserved)
}

#[derive(Clone, Copy)]
pub(crate) struct ReservedCapacity<const N: usize> {
    admitted: usize,
    admitted_by_class: [usize; N],
}

impl<const N: usize> ReservedCapacity<N> {
    pub(crate) const fn new() -> Self {
        Self {
            admitted: 0,
            admitted_by_class: [0; N],
        }
    }

    pub(crate) fn reservation_covers(
        &self,
        class_index: usize,
        amount: usize,
        reservations: &[usize; N],
    ) -> bool {
        fixed_reservation_covers(&self.admitted_by_class, class_index, amount, reservations)
    }

    pub(crate) fn try_reserve(
        &mut self,
        class_index: usize,
        amount: usize,
        capacity: usize,
        reservations: &[usize; N],
    ) -> bool {
        if !self.can_reserve(class_index, amount, capacity, reservations) {
            return false;
        }
        self.admitted = self.admitted.saturating_add(amount);
        if let Some(class_admitted) = self.admitted_by_class.get_mut(class_index) {
            *class_admitted = class_admitted.saturating_add(amount);
        }
        true
    }

    pub(crate) fn can_reserve(
        &self,
        class_index: usize,
        amount: usize,
        capacity: usize,
        reservations: &[usize; N],
    ) -> bool {
        fair_reservation_fits(
            &self.admitted_by_class,
            self.admitted,
            class_index,
            amount,
            capacity,
            reservations,
        )
    }

    pub(crate) fn release(&mut self, class_index: usize, amount: usize) {
        let Some(next_class) = self
            .admitted_by_class
            .get(class_index)
            .and_then(|admitted| admitted.checked_sub(amount))
        else {
            return;
        };
        let Some(next_total) = self.admitted.checked_sub(amount) else {
            return;
        };
        let Some(class_admitted) = self.admitted_by_class.get_mut(class_index) else {
            return;
        };
        *class_admitted = next_class;
        self.admitted = next_total;
    }

    pub(crate) const fn admitted(self) -> usize {
        self.admitted
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CountedReservedCapacity<const N: usize> {
    counts: ReservedCapacity<N>,
    bytes: ReservedCapacity<N>,
}

impl<const N: usize> CountedReservedCapacity<N> {
    pub(crate) const fn new() -> Self {
        Self {
            counts: ReservedCapacity::new(),
            bytes: ReservedCapacity::new(),
        }
    }

    pub(crate) fn reservation_covers(
        &self,
        class_index: usize,
        bytes: usize,
        count_reservations: &[usize; N],
        byte_reservations: &[usize; N],
    ) -> bool {
        self.counts
            .reservation_covers(class_index, 1, count_reservations)
            && self
                .bytes
                .reservation_covers(class_index, bytes, byte_reservations)
    }

    pub(crate) fn try_reserve(
        &mut self,
        class_index: usize,
        bytes: usize,
        count_capacity: usize,
        count_reservations: &[usize; N],
        byte_capacity: usize,
        byte_reservations: &[usize; N],
    ) -> Result<(), CountedReservationRejection> {
        if !self
            .counts
            .try_reserve(class_index, 1, count_capacity, count_reservations)
        {
            return Err(CountedReservationRejection::Count);
        }
        if !self
            .bytes
            .try_reserve(class_index, bytes, byte_capacity, byte_reservations)
        {
            self.counts.release(class_index, 1);
            return Err(CountedReservationRejection::Bytes);
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, class_index: usize, bytes: usize) {
        self.counts.release(class_index, 1);
        self.bytes.release(class_index, bytes);
    }

    pub(crate) const fn admitted_count(self) -> usize {
        self.counts.admitted()
    }

    #[cfg(test)]
    pub(crate) const fn admitted_bytes(self) -> usize {
        self.bytes.admitted()
    }
}

impl<const N: usize> Default for CountedReservedCapacity<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct FairWaitBudgetState {
    waiters: usize,
    cost: usize,
}

pub(crate) struct FairWaitBudget {
    state: Mutex<FairWaitBudgetState>,
    max_waiters: usize,
    max_cost: usize,
}

impl FairWaitBudget {
    pub(crate) fn new(max_waiters: usize, max_cost: usize) -> Self {
        Self {
            state: Mutex::new(FairWaitBudgetState::default()),
            max_waiters,
            max_cost,
        }
    }

    fn try_acquire(self: &Arc<Self>, cost: usize) -> Option<FairWaitBudgetPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next_waiters = state.waiters.checked_add(1)?;
        let next_cost = state.cost.checked_add(cost)?;
        if next_waiters > self.max_waiters || next_cost > self.max_cost {
            return None;
        }
        state.waiters = next_waiters;
        state.cost = next_cost;
        Some(FairWaitBudgetPermit {
            budget: self.clone(),
            cost,
        })
    }
}

struct FairWaitBudgetPermit {
    budget: Arc<FairWaitBudget>,
    cost: usize,
}

impl Drop for FairWaitBudgetPermit {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiters = state.waiters.saturating_sub(1);
        state.cost = state.cost.saturating_sub(self.cost);
    }
}

struct FairWaiterEntry {
    id: u64,
    waker: Option<Waker>,
    armed: bool,
    _budget: FairWaitBudgetPermit,
}

#[derive(Default)]
struct FairWaitQueueState {
    next_id: u64,
    queue: VecDeque<FairWaiterEntry>,
}

impl FairWaitQueueState {
    fn arm_front(&mut self) -> Option<Waker> {
        let waiter = self.queue.front_mut()?;
        if waiter.armed {
            return None;
        }
        waiter.armed = true;
        waiter.waker.clone()
    }
}

pub(crate) struct FairWaitQueue {
    state: Mutex<FairWaitQueueState>,
    budget: Arc<FairWaitBudget>,
}

enum FairAdmission<T> {
    Ready(T),
    Waiting(FairWaiter),
}

impl FairWaitQueue {
    pub(crate) fn with_budget(budget: Arc<FairWaitBudget>) -> Self {
        Self {
            state: Mutex::new(FairWaitQueueState::default()),
            budget,
        }
    }

    pub(crate) fn wake_front(&self) {
        let waker = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .arm_front();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn try_admit_unqueued<T, E>(
        &self,
        blocked_error: E,
        attempt: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.queue.is_empty() {
            return Err(blocked_error);
        }
        attempt()
    }

    fn admit_or_wait<T, E>(
        self: &Arc<Self>,
        cost: usize,
        budget_error: E,
        attempt: impl FnOnce() -> Option<T>,
    ) -> std::result::Result<FairAdmission<T>, E> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.queue.is_empty() {
            if let Some(value) = attempt() {
                return Ok(FairAdmission::Ready(value));
            }
        }
        let budget = self.budget.try_acquire(cost).ok_or(budget_error)?;
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.queue.push_back(FairWaiterEntry {
            id,
            waker: None,
            armed: false,
            _budget: budget,
        });
        Ok(FairAdmission::Waiting(FairWaiter {
            queue: self.clone(),
            id,
            active: true,
        }))
    }
}

pub(crate) async fn acquire_fair<T>(
    queue: &Arc<FairWaitQueue>,
    cost: usize,
    budget_error: crate::error::Error,
    closed_error: impl Fn() -> crate::error::Error,
    attempt: impl FnMut() -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    let mut attempt = attempt;
    let mut try_acquire = || attempt().ok();
    match queue.admit_or_wait(cost, budget_error, &mut try_acquire)? {
        FairAdmission::Ready(value) => Ok(value),
        FairAdmission::Waiting(mut waiter) => {
            poll_fn(|context| match waiter.poll(context, &mut try_acquire) {
                Poll::Ready(Some(value)) => Poll::Ready(Ok(value)),
                Poll::Ready(None) => Poll::Ready(Err(closed_error())),
                Poll::Pending => Poll::Pending,
            })
            .await
        }
    }
}

struct FairWaiter {
    queue: Arc<FairWaitQueue>,
    id: u64,
    active: bool,
}

impl FairWaiter {
    fn poll<T>(
        &mut self,
        context: &mut Context<'_>,
        attempt: impl FnOnce() -> Option<T>,
    ) -> Poll<Option<T>> {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(position) = state.queue.iter().position(|waiter| waiter.id == self.id) else {
            return Poll::Ready(None);
        };
        let Some(waiter) = state.queue.get_mut(position) else {
            return Poll::Ready(None);
        };
        waiter.waker = Some(context.waker().clone());
        if position != 0 {
            return Poll::Pending;
        }
        if !waiter.armed {
            return Poll::Pending;
        }
        waiter.armed = false;
        let Some(value) = attempt() else {
            return Poll::Pending;
        };
        state.queue.pop_front();
        let next = state.arm_front();
        self.active = false;
        drop(state);
        if let Some(waker) = next {
            waker.wake();
        }
        Poll::Ready(Some(value))
    }

    fn cancel(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(position) = state.queue.iter().position(|waiter| waiter.id == self.id) else {
            self.active = false;
            return;
        };
        let was_front = position == 0;
        state.queue.remove(position);
        self.active = false;
        let next = was_front.then(|| state.arm_front()).flatten();
        drop(state);
        if let Some(waker) = next {
            waker.wake();
        }
    }
}

impl Drop for FairWaiter {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod fair_wait_queue_tests;

#[cfg(test)]
mod reserved_capacity_tests {
    use super::admissible_capacity;
    use super::ReservedCapacity;

    #[test]
    fn admissible_capacity_preserves_every_other_class_reservation() {
        let reservations = [1, 2, 3];

        assert_eq!(admissible_capacity(10, &reservations, 0), 5);
        assert_eq!(admissible_capacity(10, &reservations, 1), 6);
        assert_eq!(admissible_capacity(10, &reservations, 2), 7);
        assert_eq!(admissible_capacity(10, &reservations, 3), 4);
    }

    #[test]
    fn invalid_release_keeps_aggregate_and_class_totals_equal() {
        let mut capacity = ReservedCapacity::<2>::new();
        assert!(capacity.try_reserve(0, 4, 8, &[0, 0]));
        capacity.release(2, 1);
        assert_eq!(capacity.admitted, 4);
        assert_eq!(capacity.admitted_by_class, [4, 0]);
        capacity.release(0, 5);
        assert_eq!(capacity.admitted, 4);
        assert_eq!(capacity.admitted_by_class, [4, 0]);
    }

    #[test]
    fn valid_release_preserves_capacity_sum_invariant() {
        let mut capacity = ReservedCapacity::<2>::new();
        assert!(capacity.try_reserve(0, 3, 8, &[0, 0]));
        assert!(capacity.try_reserve(1, 2, 8, &[0, 0]));
        capacity.release(0, 2);
        assert_eq!(capacity.admitted, 3);
        assert_eq!(capacity.admitted_by_class.iter().sum::<usize>(), 3);
    }
}
