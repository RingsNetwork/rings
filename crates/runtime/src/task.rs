//! Handing work to the executor.
//!
//! Two contracts, deliberately kept apart:
//!
//! ```text
//!   spawn_detached : F → Result<(), Unscheduled F>        fire and forget
//!   run_detached   : F → Future (Result<T, DetachedError>) awaited, but owned by the runtime
//! ```
//!
//! Both hand ownership of the future to the executor at the call, not at a later poll:
//! cancelling the caller (dropping the future that awaits [`run_detached`]) never cancels
//! the work. Neither returns a handle that aborts the work — work whose lifetime the caller
//! must own is not *detached*, and a caller that needs it keeps its own abort handle
//! (native-only modules do so with Tokio's `JoinHandle`) instead of reaching for this
//! boundary.
//!
//! Scheduling is fallible. Native code needs a current Tokio runtime; the browser event loop
//! is always current. A missing runtime is reported *before* anything starts:
//!
//! * [`Spawner::current`] fails with [`RuntimeUnavailable`], so a caller that claims a
//!   resource for the task acquires the spawner first and then spawns infallibly;
//! * [`spawn_detached`] hands the unpolled future back in [`Unscheduled`], so a caller whose
//!   task must not be lost can run it inline instead.

use std::fmt;
use std::future::Future;

use futures::channel::oneshot;

use crate::bound::MaybeSend;

/// No executor is current on this thread, so nothing can be scheduled on it.
///
/// Only native code observes this: the browser event loop is always current.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("no async runtime is current on this thread")]
pub struct RuntimeUnavailable;

/// A future that [`spawn_detached`] could not schedule, returned unpolled.
pub struct Unscheduled<F>(F);

impl<F> Unscheduled<F> {
    /// Recover the future; it has never been polled.
    pub fn into_inner(self) -> F {
        self.0
    }
}

impl<F> fmt::Debug for Unscheduled<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Unscheduled(..)")
    }
}

/// Scheduled work ended without publishing its output: it panicked, or its runtime shut down.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("detached task ended before publishing its result")]
pub struct Abandoned;

/// Why [`run_detached`] produced no output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DetachedError {
    /// No executor was current, so the work never started.
    #[error(transparent)]
    Unavailable(#[from] RuntimeUnavailable),
    /// The work started but published nothing.
    #[error(transparent)]
    Abandoned(#[from] Abandoned),
}

/// The capability to schedule detached work on one executor.
///
/// Acquired once with [`Spawner::current`]; afterwards [`Spawner::spawn`] needs no runtime
/// on the calling thread (the native spawner pins the executor it was acquired on).
///
/// Invariant: a spawner does not keep its executor alive. Once a native runtime has shut
/// down, work handed to its spawner is dropped unpolled — see [`Spawner::spawn`].
#[derive(Clone, Debug)]
pub struct Spawner {
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    handle: tokio::runtime::Handle,
}

impl Spawner {
    /// The executor current on this thread.
    ///
    /// Post: `Err` iff native code runs outside a Tokio runtime.
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    pub fn current() -> Result<Self, RuntimeUnavailable> {
        tokio::runtime::Handle::try_current()
            .map(|handle| Self { handle })
            .map_err(|_| RuntimeUnavailable)
    }

    /// The browser event loop, which is always current.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    pub fn current() -> Result<Self, RuntimeUnavailable> {
        Ok(Self {})
    }

    /// Run `future` detached on this spawner's executor.
    ///
    /// Post: the executor owns `future` and runs it to completion while it is alive. If the
    /// executor has shut down, `future` is dropped unpolled and `spawn` cannot report it;
    /// work whose loss must be observed goes through [`Spawner::run_detached`], which reports
    /// it as [`Abandoned`].
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    pub fn spawn<F>(&self, future: F)
    where F: Future<Output = ()> + MaybeSend + 'static {
        // Dropping the join handle detaches the task; it is not cancelled.
        drop(self.handle.spawn(future));
    }

    /// Run `future` detached on the browser event loop.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    pub fn spawn<F>(&self, future: F)
    where F: Future<Output = ()> + MaybeSend + 'static {
        wasm_bindgen_futures::spawn_local(future);
    }

    /// Start `future` on this spawner's executor now and return a future of its output.
    ///
    /// Law: the executor, not the returned future, owns the work — dropping the returned
    /// future abandons only the wait, and the work still runs to completion.
    pub fn run_detached<F, T>(
        &self,
        future: F,
    ) -> impl Future<Output = Result<T, Abandoned>> + MaybeSend + 'static
    where
        F: Future<Output = T> + MaybeSend + 'static,
        T: MaybeSend + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.spawn(async move {
            // The receiver is gone only when the waiter was dropped; the work has run anyway.
            let _ = sender.send(future.await);
        });
        async move { receiver.await.map_err(|_| Abandoned) }
    }
}

/// Run `future` detached from the caller on the current executor.
///
/// Post: `Ok` means the executor owns the future; `Err` hands it back unpolled because no
/// executor is current, and the caller runs it inline or refuses the work.
pub fn spawn_detached<F>(future: F) -> Result<(), Unscheduled<F>>
where F: Future<Output = ()> + MaybeSend + 'static {
    match Spawner::current() {
        Ok(spawner) => {
            spawner.spawn(future);
            Ok(())
        }
        Err(RuntimeUnavailable) => Err(Unscheduled(future)),
    }
}

/// Start `future` on the current executor now and return a future of its output.
///
/// Post: the returned future yields `Unavailable` at once when no executor is current;
/// otherwise [`Spawner::run_detached`]'s ownership law holds.
pub fn run_detached<F, T>(
    future: F,
) -> impl Future<Output = Result<T, DetachedError>> + MaybeSend + 'static
where
    F: Future<Output = T> + MaybeSend + 'static,
    T: MaybeSend + 'static,
{
    let output = Spawner::current().map(|spawner| spawner.run_detached(future));
    async move { Ok(output?.await?) }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::run_detached;
    use super::spawn_detached;
    use super::Abandoned;
    use super::DetachedError;
    use super::RuntimeUnavailable;
    use super::Spawner;

    #[test]
    fn test_spawner_outside_a_runtime_is_unavailable() {
        assert_eq!(Spawner::current().err(), Some(RuntimeUnavailable));
    }

    #[test]
    fn test_unscheduled_future_is_handed_back_unpolled() {
        let polled = Arc::new(AtomicBool::new(false));
        let witness = Arc::clone(&polled);
        let unscheduled = spawn_detached(async move { witness.store(true, Ordering::SeqCst) })
            .expect_err("no runtime is current outside Tokio");

        assert!(!polled.load(Ordering::SeqCst));
        futures::executor::block_on(unscheduled.into_inner());
        assert!(polled.load(Ordering::SeqCst));
    }

    #[test]
    fn test_run_detached_outside_a_runtime_is_unavailable() {
        let result = futures::executor::block_on(run_detached(async {}));

        assert_eq!(result, Err(DetachedError::Unavailable(RuntimeUnavailable)));
    }

    #[test]
    fn test_spawner_keeps_its_executor_on_a_runtime_less_thread() {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let spawner = runtime
            .block_on(async { Spawner::current() })
            .expect("current");
        let done = Arc::new(Notify::new());
        let signal = Arc::clone(&done);

        std::thread::spawn(move || spawner.spawn(async move { signal.notify_one() }))
            .join()
            .expect("spawning thread");

        runtime.block_on(done.notified());
    }

    #[tokio::test]
    async fn test_cancelling_waiter_does_not_cancel_owned_detached_work() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        let waiter = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let completed = Arc::clone(&completed);
            tokio::spawn(run_detached(async move {
                started.notify_one();
                release.notified().await;
                completed.notify_one();
            }))
        };

        started.notified().await;
        waiter.abort();
        let _ = waiter.await;
        release.notify_one();

        tokio::time::timeout(Duration::from_secs(1), completed.notified())
            .await
            .expect("owned detached work must outlive its cancelled waiter");
    }

    #[tokio::test]
    async fn test_work_starts_at_the_call_not_at_the_first_poll() {
        let completed = Arc::new(Notify::new());
        let signal = Arc::clone(&completed);

        drop(run_detached(async move { signal.notify_one() }));

        tokio::time::timeout(Duration::from_secs(1), completed.notified())
            .await
            .expect("work handed to the executor must run although its waiter was never polled");
    }

    /// Law: a spawner outliving its runtime drops work unpolled, and `run_detached` reports
    /// the loss as `Abandoned` instead of hanging.
    #[test]
    fn test_spawner_after_runtime_shutdown_drops_work_and_reports_abandoned() {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let spawner = runtime
            .block_on(async { Spawner::current() })
            .expect("current");
        drop(runtime);
        let polled = Arc::new(AtomicBool::new(false));
        let witness = Arc::clone(&polled);

        spawner.spawn(async move { witness.store(true, Ordering::SeqCst) });
        let result = futures::executor::block_on(spawner.run_detached(async {}));

        assert!(!polled.load(Ordering::SeqCst));
        assert_eq!(result, Err(Abandoned));
    }

    #[tokio::test]
    async fn test_panicking_detached_work_is_abandoned() {
        let result: Result<(), DetachedError> = run_detached(async {
            panic!("injected detached task failure");
        })
        .await;

        assert_eq!(result, Err(DetachedError::Abandoned(Abandoned)));
    }
}
