//! Whether a node's registrations are stored at their owners, as the owners reported it (#866).
//!
//! Each registration task has a latch. The first publish that began with an admitted edge and
//! whose every remote placement's owner reported it stored sets it
//! (`rings_core::swarm::storage_api::StoreReceipt`). A publish that began with no edge sets
//! nothing: it settled at the node alone and reached no owner (#855). A latch never clears.
//! Readiness means the registry was reachable at its owners once; the lease heartbeat keeps it
//! listed after that (#918).
//!
//! ```text
//!   ready(i) ⟺ ∃ publish p of task i: edges(p) ≥ 1 ∧ ∀ remote placement q of p: reported(q, Stored)
//!   all()    resolves once ∀ i: ready(i)          (at once for a node with no registration task)
//! ```
//!
//! Law (one wait): a newer wait of a task ends its older one, so reports that are lost leave
//! at most one wait per task.

use std::convert::Infallible;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use futures::channel::oneshot;
use futures::future::join_all;
use futures::future::select;
use futures::future::Either;
use futures::future::Shared;
use futures::pin_mut;
use futures::FutureExt;
use rings_core::swarm::storage_api::StoreUnconfirmed;

/// The latch of one registration task.
struct Latch {
    /// Whether the task is ready; raised with the send of `set`.
    raised: AtomicBool,
    /// Sends once, when the task is ready.
    set: Mutex<Option<oneshot::Sender<()>>>,
    /// Resolves once the task is ready.
    ready: Shared<oneshot::Receiver<()>>,
    /// The supersession signal of the task's current wait: dropping it ends that wait.
    current: Mutex<Option<oneshot::Sender<Infallible>>>,
}

/// The value of `mutex`, for one pure step. A poisoned lock still guards a well-formed value:
/// no step panics between two writes.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether each registration task of a node is stored at its owners (module laws).
pub(crate) struct RegistrationReadiness {
    latches: Vec<Latch>,
}

impl RegistrationReadiness {
    /// The readiness of `tasks` registration tasks, none ready.
    pub(crate) fn new(tasks: usize) -> Self {
        let latches = (0..tasks)
            .map(|_| {
                let (set, ready) = oneshot::channel();
                Latch {
                    raised: AtomicBool::new(false),
                    set: Mutex::new(Some(set)),
                    ready: ready.shared(),
                    current: Mutex::new(None),
                }
            })
            .collect();
        Self { latches }
    }

    /// Whether the registration task numbered `task` is ready.
    pub(crate) fn is_ready(&self, task: usize) -> bool {
        self.latches
            .get(task)
            .is_some_and(|latch| latch.raised.load(Ordering::Acquire))
    }

    /// Await the `receipts` of one publish of task `task` that began with an admitted edge:
    /// once every one resolves stored, the task is ready. The wait ends without setting
    /// anything when a receipt resolves otherwise, or when a newer wait of the task begins
    /// (the one-wait law).
    pub(crate) fn wait<R>(
        self: &Arc<Self>,
        task: usize,
        receipts: Vec<R>,
    ) -> impl Future<Output = ()> + 'static
    where
        R: Future<Output = Result<(), StoreUnconfirmed>> + 'static,
    {
        let (current, superseded) = oneshot::channel::<Infallible>();
        if let Some(latch) = self.latches.get(task) {
            *locked(&latch.current) = Some(current);
        }
        let readiness = Arc::clone(self);
        async move {
            let stored = join_all(receipts);
            pin_mut!(stored, superseded);
            if let Either::Left((outcomes, _)) = select(stored, superseded).await {
                if outcomes.iter().all(Result::is_ok) {
                    readiness.set(task);
                }
            }
        }
    }

    /// Set the latch of task `task`. Idempotent.
    fn set(&self, task: usize) {
        let Some(latch) = self.latches.get(task) else {
            return;
        };
        latch.raised.store(true, Ordering::Release);
        if let Some(sender) = locked(&latch.set).take() {
            let _ = sender.send(());
        }
    }

    /// Resolves once every registration task is ready, at once for none.
    pub(crate) fn all(self: &Arc<Self>) -> impl Future<Output = ()> + 'static {
        let readiness = Arc::clone(self);
        let latches = join_all(self.latches.iter().map(|latch| latch.ready.clone()));
        async move {
            // Held while waiting, so every latch's sender lives until it is set.
            let _readiness = readiness;
            latches.await;
        }
    }
}

#[cfg(all(test, rings_native))]
mod test_readiness;
