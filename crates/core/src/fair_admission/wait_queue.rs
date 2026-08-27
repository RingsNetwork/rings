use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

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

pub(super) enum FairAdmission<T> {
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

pub(super) struct FairWaiter {
    queue: Arc<FairWaitQueue>,
    id: u64,
    active: bool,
}

impl FairWaiter {
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
