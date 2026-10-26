//! A set of detached tasks whose end can be awaited.
//!
//! ```text
//!   spawn   : TaskSet × F → Result (Joined F::Output) (Unscheduled F)    the task joins the set
//!   drained : TaskSet → Future ()                                         no task of the set is live
//! ```
//!
//! Law (liveness count): a task is live from its spawn until its future completes or is dropped
//! (its executor shut down), and the set counts exactly its live tasks. [`TaskSet::drained`]
//! resolves at an instant the count is zero, woken by the end of the last live task, never by
//! time. A task spawned into the set by one of its live tasks is counted before its spawner
//! ends, so a set whose tasks spawn only into it drains once every one of them, children
//! included, is done.
//!
//! The set owns no task: as for [`spawn_detached`](crate::spawn_detached), the executor runs
//! each task to its end, dropping a [`Joined`] abandons only the wait, and the set cancels
//! nothing. A caller that must end its tasks gives them a way to end (a stop signal they
//! observe) and then awaits [`TaskSet::drained`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use futures::channel::oneshot;

use crate::bound::MaybeSend;
use crate::task::Abandoned;
use crate::task::Spawner;
use crate::task::Unscheduled;

/// A set of detached tasks whose end can be awaited (the liveness-count law).
///
/// Cloning shares the set: a task spawned through any clone is counted by all of them.
#[derive(Clone, Default)]
pub struct TaskSet {
    live: Arc<Live>,
}

/// The live tasks of a set and the waiters of its drain.
#[derive(Default)]
struct Live {
    state: Mutex<LiveState>,
}

/// The count of live tasks, and the wakers of the [`Drained`] futures that saw it above zero.
#[derive(Default)]
struct LiveState {
    count: usize,
    waiters: Vec<Waker>,
}

impl Live {
    /// The state, under its lock. A poisoned lock still guards a well-formed state: no step
    /// panics between two writes.
    fn state(&self) -> MutexGuard<'_, LiveState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One live task's place in its set: counted while it lives, released when it is dropped, with
/// the task's future or with the future the executor never polled.
struct Membership(Arc<Live>);

impl Membership {
    /// Count one more live task in `live`.
    fn join(live: &Arc<Live>) -> Self {
        live.state().count += 1;
        Self(Arc::clone(live))
    }
}

impl Drop for Membership {
    /// Count the task out, and wake every waiter of the drain if it was the last.
    fn drop(&mut self) {
        let waiters = {
            let mut state = self.0.state();
            state.count = state.count.saturating_sub(1);
            if state.count == 0 {
                std::mem::take(&mut state.waiters)
            } else {
                Vec::new()
            }
        };
        waiters.into_iter().for_each(Waker::wake);
    }
}

impl TaskSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `task` detached on the current executor as a member of this set.
    ///
    /// Post: `Ok` means the executor owns `task`, which is live in the set until it ends, and
    /// the [`Joined`] resolves to its output, or to [`Abandoned`] if it is dropped first.
    /// `Err` hands `task` back unpolled because no executor is current, and the set is
    /// unchanged.
    pub fn spawn<F>(&self, task: F) -> Result<Joined<F::Output>, Unscheduled<F>>
    where
        F: Future + MaybeSend + 'static,
        F::Output: MaybeSend + 'static,
    {
        match Spawner::current() {
            Ok(spawner) => Ok(self.spawn_with(&spawner, task)),
            Err(_) => Err(Unscheduled(task)),
        }
    }

    /// Run `task` detached on `spawner`'s executor as a member of this set: [`Self::spawn`]
    /// for a caller that acquired the executor before claiming anything for the task.
    ///
    /// Post: as for [`Self::spawn`]'s `Ok`; if the executor has shut down, `task` is dropped
    /// unpolled, leaves the set at once, and its [`Joined`] is [`Abandoned`].
    pub fn spawn_with<F>(&self, spawner: &Spawner, task: F) -> Joined<F::Output>
    where
        F: Future + MaybeSend + 'static,
        F::Output: MaybeSend + 'static,
    {
        let membership = Membership::join(&self.live);
        let (sender, receiver) = oneshot::channel();
        spawner.spawn(async move {
            let _membership = membership;
            // The receiver is gone only when the wait was abandoned; the task has run anyway.
            let _ = sender.send(task.await);
        });
        Joined(receiver)
    }

    /// Resolves at an instant no task of this set is live (the liveness-count law).
    pub fn drained(&self) -> Drained {
        Drained(Arc::clone(&self.live))
    }

    /// How many tasks of this set are live now.
    pub fn live(&self) -> usize {
        self.live.state().count
    }
}

/// The output of a task of a [`TaskSet`]: its value, or [`Abandoned`] if the task was dropped
/// before it ended. Dropping it abandons only the wait.
pub struct Joined<T>(oneshot::Receiver<T>);

impl<T> Future for Joined<T> {
    type Output = Result<T, Abandoned>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map_err(|_| Abandoned)
    }
}

/// Resolves at an instant no task of its [`TaskSet`] is live.
pub struct Drained(Arc<Live>);

impl Future for Drained {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.0.state();
        if state.count == 0 {
            return Poll::Ready(());
        }
        if !state.waiters.iter().any(|waiter| waiter.will_wake(cx.waker())) {
            state.waiters.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test_task_set;
