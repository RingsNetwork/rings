use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use crate::lifecycle::epoch::Epoch;

/// Mutable counters guarded by [`FairWaitBudget`].
#[derive(Default)]
struct FairWaitBudgetState {
    /// Current number of active waiters holding a budget permit.
    waiters: usize,
    /// Current total cost reserved by active waiters.
    cost: usize,
}

/// Shared waiter-count and retained-cost budget for one or more [`FairWaitQueue`] instances.
pub(crate) struct FairWaitBudget {
    /// Synchronized usage counters.
    state: Mutex<FairWaitBudgetState>,
    /// Maximum concurrent waiters allowed.
    max_waiters: usize,
    /// Maximum cumulative waiter cost allowed.
    max_cost: usize,
}

impl FairWaitBudget {
    /// Create a budget allowing at most `max_waiters` waiters and `max_cost` total cost.
    pub(crate) fn new(max_waiters: usize, max_cost: usize) -> Self {
        Self {
            state: Mutex::new(FairWaitBudgetState::default()),
            max_waiters,
            max_cost,
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn try_acquire_for_test(
        self: &Arc<Self>,
        cost: usize,
    ) -> Option<FairWaitBudgetPermit> {
        self.try_acquire(cost)
    }

    /// Try to reserve one waiter slot and `cost` from this budget.
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

/// RAII permit accounting for one queued waiter's share of a [`FairWaitBudget`].
pub(super) struct FairWaitBudgetPermit {
    /// Budget to credit when this permit is dropped.
    budget: Arc<FairWaitBudget>,
    /// Cost reserved by this permit.
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

/// Queue entry representing one waiting caller.
struct FairWaiterEntry {
    /// Monotonic identifier matching the corresponding [`FairWaiter`].
    id: u64,
    /// Latest task waker registered when the waiter was polled.
    waker: Option<Waker>,
    /// Whether this front waiter has been armed to retry admission on its next poll.
    armed: bool,
    /// Held budget permit released when the entry leaves the queue.
    _budget: FairWaitBudgetPermit,
}

/// Mutable FIFO waiter queue state.
#[derive(Default)]
struct FairWaitQueueState {
    /// Next waiter identifier to assign.
    next_id: u64,
    /// FIFO queue of waiting callers.
    queue: VecDeque<FairWaiterEntry>,
}

impl FairWaitQueueState {
    /// Arm the front waiter if not already armed and return its waker for notification.
    fn arm_front(&mut self) -> Option<Waker> {
        let waiter = self.queue.front_mut()?;
        if waiter.armed {
            return None;
        }
        waiter.armed = true;
        waiter.waker.clone()
    }
}

/// Bounded FIFO wait queue ensuring waiters are admitted in order without starving behind new arrivals.
pub(crate) struct FairWaitQueue {
    /// Synchronized FIFO waiter queue.
    state: Mutex<FairWaitQueueState>,
    /// Budget capping total queued waiters and retained cost.
    budget: Arc<FairWaitBudget>,
    /// Advanced whenever a waiter leaves the queue, admitted or cancelled: the event after
    /// which an unqueued admission may pass again.
    departures: Epoch,
}

/// Immediate outcome of attempting admission through [`FairWaitQueue::admit_or_wait`].
pub(super) enum FairAdmission<T> {
    /// Admission succeeded immediately without queuing.
    Ready(T),
    /// Caller was enqueued and must poll the returned [`FairWaiter`].
    Waiting(FairWaiter),
}

impl FairWaitQueue {
    /// Create an empty wait queue bounded by `budget`.
    pub(crate) fn with_budget(budget: Arc<FairWaitBudget>) -> Self {
        Self {
            state: Mutex::new(FairWaitQueueState::default()),
            budget,
            departures: Epoch::default(),
        }
    }

    /// Whether no waiter is queued, so an unqueued admission would be attempted.
    pub(crate) fn is_empty(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .is_empty()
    }

    /// The epoch of departures from the queue.
    pub(crate) const fn departures(&self) -> &Epoch {
        &self.departures
    }

    /// Arm and wake the front waiter, if any, so it can retry admission.
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

    /// Attempt `attempt` only when no waiters are queued; otherwise return `blocked_error`.
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

    /// Try immediate admission when the queue is empty, or enqueue a [`FairWaiter`] charged `cost`.
    pub(super) fn admit_or_wait<T, E>(
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

/// Acquire a resource fairly through `queue`, waiting in FIFO order if immediate admission fails.
pub(crate) async fn acquire_fair<T>(
    queue: &Arc<FairWaitQueue>,
    cost: usize,
    budget_error: crate::error::Error,
    closed_error: impl Fn() -> crate::error::Error,
    attempt: impl FnMut() -> Option<T>,
) -> crate::error::Result<T> {
    let mut attempt = attempt;
    match queue.admit_or_wait(cost, budget_error, &mut attempt)? {
        FairAdmission::Ready(value) => Ok(value),
        FairAdmission::Waiting(mut waiter) => {
            poll_fn(|context| match waiter.poll(context, &mut attempt) {
                Poll::Ready(Some(value)) => Poll::Ready(Ok(value)),
                Poll::Ready(None) => Poll::Ready(Err(closed_error())),
                Poll::Pending => Poll::Pending,
            })
            .await
        }
    }
}

/// Handle representing a caller queued in a [`FairWaitQueue`]; dropping it cancels the reservation.
pub(super) struct FairWaiter {
    /// Owning wait queue.
    queue: Arc<FairWaitQueue>,
    /// Unique identifier of this waiter's queue entry.
    id: u64,
    /// Whether this waiter is still active and needs cancellation on drop.
    active: bool,
}

impl FairWaiter {
    /// Poll for admission once this waiter reaches the front of the queue and is armed.
    pub(super) fn poll<T>(
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
        self.queue.departures.advance();
        if let Some(waker) = next {
            waker.wake();
        }
        Poll::Ready(Some(value))
    }

    /// Remove this waiter from the queue if still active and wake the next front waiter if needed.
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
        self.queue.departures.advance();
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
