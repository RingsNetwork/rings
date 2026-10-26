//! The node's own tasks (#1085): one stop that ends every task the node layer starts, and one
//! set they all join, so a close can stop them and await their end.
//!
//! Law (one spawn): every task of the node layer is spawned through [`NodeTasks`], onto the
//! node's task set. `clippy.toml` forbids this crate every detached spawn of an executor
//! (`tokio::spawn`, `spawn_local`, the spawns of `rings_runtime`), outside test builds; the
//! task set's spawn, which only this module calls, is the one left. So no task of the node can
//! outlive a close unnoticed.
//!
//! ```text
//!   spawn(t)              : t runs until it ends or the node stops, whichever comes first;
//!                           at the stop it is dropped at its next suspension point
//!   spawn_winding_down(t) : t observes the stop itself, and ends within a bounded time after it
//!   stop()                : every node task ends
//!   drained               : resolves at an instant no node task is live, woken by the last
//!                           one's end
//! ```
//!
//! A task dropped at the stop yields `None`, one that ended on its own `Some`.
//!
//! A task that must finish its own end rather than be dropped is spawned winding down: a relay
//! whose fail-closed law resets its local socket on any end but a clean one (`onion::tcp`, the
//! relay engine's TCP relay), since a dropped socket closes cleanly. Two long-running tasks are
//! not node tasks: the measurement's persistence worker ends with its owner, and the listener
//! generation, which a caller runs, winds down on the stop it observes.

use std::future::Future;

use futures::future::select;
use futures::future::Either;
use futures::pin_mut;
use rings_core::lifecycle::StopSource;
use rings_core::lifecycle::StopToken;
use rings_runtime::Drained;
use rings_runtime::Joined;
use rings_runtime::MaybeSend;
use rings_runtime::RuntimeUnavailable;
use rings_runtime::Spawner;
use rings_runtime::TaskSet;

/// The stop and the task set of one node, shared by every clone of its processor.
#[derive(Clone, Default)]
pub(crate) struct NodeTasks {
    stop: StopSource,
    tasks: TaskSet,
}

/// The capability to spawn node tasks on one executor, acquired before a caller claims
/// anything for its task, so a missing executor claims nothing.
#[derive(Clone)]
pub(crate) struct NodeSpawner {
    tasks: NodeTasks,
    spawner: Spawner,
}

impl NodeTasks {
    /// A node with no task, not stopped.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The capability to spawn node tasks on the current executor.
    ///
    /// # Errors
    ///
    /// [`RuntimeUnavailable`] when no executor is current.
    pub(crate) fn spawner(&self) -> Result<NodeSpawner, RuntimeUnavailable> {
        Ok(NodeSpawner {
            tasks: self.clone(),
            spawner: Spawner::current()?,
        })
    }

    /// Run `task` as a node task on the current executor; see [`NodeSpawner::spawn`].
    ///
    /// # Errors
    ///
    /// [`RuntimeUnavailable`] when no executor is current; nothing is spawned.
    pub(crate) fn spawn<F>(&self, task: F) -> Result<Joined<Option<F::Output>>, RuntimeUnavailable>
    where
        F: Future + MaybeSend + 'static,
        F::Output: MaybeSend + 'static,
    {
        Ok(self.spawner()?.spawn(task))
    }

    /// The node's stop, which a long-running task of the node that is not a node task (the
    /// listener generation) observes.
    pub(crate) fn stop_token(&self) -> StopToken {
        self.stop.token()
    }

    /// Stop the node: every node task ends at its next suspension point. Idempotent.
    pub(crate) fn stop(&self) {
        self.stop.request_stop();
    }

    /// Resolves at an instant no node task is live (the liveness-count law of [`TaskSet`]).
    pub(crate) fn drained(&self) -> Drained {
        self.tasks.drained()
    }
}

impl NodeSpawner {
    /// Run `task` until it ends or the node stops, whichever comes first, as a member of the
    /// node's task set: for a task whose drop at any suspension point is a clean end.
    ///
    /// Post: the [`Joined`] yields `Some(output)` if `task` ended on its own, `None` if the stop
    /// dropped it first, and `Abandoned` if its executor shut down.
    pub(crate) fn spawn<F>(&self, task: F) -> Joined<Option<F::Output>>
    where
        F: Future + MaybeSend + 'static,
        F::Output: MaybeSend + 'static,
    {
        let stop = self.tasks.stop.token();
        self.tasks.tasks.spawn_with(&self.spawner, async move {
            let stopped = stop.stopped();
            pin_mut!(task, stopped);
            match select(task, stopped).await {
                Either::Left((output, _)) => Some(output),
                Either::Right(((), _)) => None,
            }
        })
    }

    /// Run the task `task(stop)`, which observes the node's stop itself, as a member of the
    /// node's task set: for a task that must finish its own end at the stop (reset a socket it
    /// holds, tear down its session) rather than be dropped.
    ///
    /// Pre: once `stop` fires, the task ends within a bounded time, awaiting after it nothing
    /// but bounded steps, so the drain of a close stays bounded.
    /// Post: the [`Joined`] yields the task's output, or `Abandoned` if its executor shut down.
    ///
    /// Native only: the tasks that hold a socket are the native relays.
    #[cfg(rings_native)]
    pub(crate) fn spawn_winding_down<T, F>(&self, task: T) -> Joined<F::Output>
    where
        T: FnOnce(StopToken) -> F,
        F: Future + MaybeSend + 'static,
        F::Output: MaybeSend + 'static,
    {
        self.tasks
            .tasks
            .spawn_with(&self.spawner, task(self.tasks.stop.token()))
    }
}

/// Run `task` to its end; if `stop` fires first, call `wind_down` once, which makes `task` end
/// itself, and keep awaiting it: a task winding down through a cancel of its own.
#[cfg(rings_native)]
pub(crate) async fn wound_down<F, W>(stop: StopToken, task: F, wind_down: W) -> F::Output
where
    F: Future,
    W: FnOnce(),
{
    let stopped = stop.stopped();
    pin_mut!(task, stopped);
    match select(task, stopped).await {
        Either::Left((output, _)) => output,
        Either::Right(((), task)) => {
            wind_down();
            task.await
        }
    }
}
