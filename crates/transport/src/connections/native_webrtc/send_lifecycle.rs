//! One retirement authority shared by the caller and its detached send owner.
//!
//! State: `Armed(close) -> Closing -> Finished`. Only a failure observed while
//! admission is irrevocable may consume `close`. Logical fencing precedes that
//! transition; physical completion is a separate witness owned by the backend.
//! A successful observation does not undo another observer's retirement decision.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use tokio_util::sync::CancellationToken;

use super::send_runtime::NativeRetirementFence;
use crate::core::drop_guard::ArmedDropGuard;
use crate::core::transport::SendAcceptance;
use crate::error::Result;
use crate::sync_utils::lock_recover;

/// Owned, generation-pinned physical close operation; never cloned.
type PhysicalClose = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// The unique close capability and the completion signal for one send.
///
/// `Arc` observers share this authority; they do not acquire independent close
/// capabilities. Taking `close` is the only transition that starts cleanup.
pub(super) struct SendLifecycle {
    /// Executor which continues cleanup even if the caller disappears.
    runtime: tokio::runtime::Handle,
    /// Existing atomic admission state; acceptance is not duplicated here.
    acceptance: SendAcceptance,
    /// Connection-generation gate closed synchronously before cleanup starts.
    fence: NativeRetirementFence,
    /// `Some` is Armed; `None` is Closing or Finished, distinguished by `finished`.
    close: Mutex<Option<PhysicalClose>>,
    /// Cleanup task termination, including runtime cancellation, not proof of close success.
    finished: CancellationToken,
}

impl SendLifecycle {
    /// Create one authority before constructing or polling the send operation.
    pub(super) fn new(
        runtime: tokio::runtime::Handle,
        acceptance: SendAcceptance,
        fence: NativeRetirementFence,
        close: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            acceptance,
            fence,
            close: Mutex::new(Some(Box::pin(close))),
            finished: CancellationToken::new(),
        })
    }

    /// Fence an uncertain write and transfer the unique close capability to the executor.
    ///
    /// Multiple observers may race here. All fence before returning, but only the
    /// observer consuming `close` can spawn cleanup. Acceptance racing after this
    /// failure observation cannot reopen the generation or cancel its cleanup.
    pub(super) fn fail(&self) {
        if !self.acceptance.failed_after_irrevocable() {
            return;
        }
        self.fence.request();
        // The capability is removed under the lock; spawning and user cleanup
        // happen outside it so neither can re-enter a held lifecycle lock.
        let close = lock_recover(&self.close).take();
        if let Some(close) = close {
            // Construct before spawn: even an unpolled task dropped by runtime
            // shutdown releases waiters. The physical-close witness stays false.
            let completion = ArmedDropGuard::new(self.finished.clone(), |done| done.cancel());
            self.runtime.spawn(async move {
                let _completion = completion;
                if let Err(error) = close.await {
                    tracing::warn!(%error, "failed to retire native send generation");
                }
            });
        }
    }

    /// Wait for previously started cleanup without taking ownership of its task.
    pub(super) async fn wait_for_cleanup(&self) {
        // Failure callers invoke `fail` before this method. Cancellation during
        // this wait cannot discard the close operation, already owned by Tokio.
        let closing = lock_recover(&self.close).is_none();
        if closing {
            self.finished.cancelled().await;
        }
    }
}

/// Owns a send future across inline polling, task handoff, and destruction.
///
/// Drop reports abandonment before Rust drops `future`. Both caller and worker
/// use this boundary, sharing one lifecycle rather than stacking independent
/// retirement actions. No guard is stored inside the primitive's captures.
pub(super) struct OwnedSend<F: Future> {
    /// Pinned resource owner, dropped only after the custom Drop boundary.
    future: Pin<Box<F>>,
    /// Shared observation rights to the unique retirement authority.
    lifecycle: Arc<SendLifecycle>,
    /// A terminal result has already reported its failure, if any.
    completed: bool,
}

impl<F: Future> OwnedSend<F> {
    /// Take ownership before the future can cross an irrevocable boundary.
    pub(super) fn new(future: F, lifecycle: Arc<SendLifecycle>) -> Self {
        Self {
            future: Box::pin(future),
            lifecycle,
            completed: false,
        }
    }

    /// Poll beneath an admission lock, releasing it before reporting failure or panic.
    pub(super) fn poll_admitted<G, T>(&mut self, admission: G) -> Poll<Result<T>>
    where F: Future<Output = Result<T>> {
        // Catch while retaining the future so a panic cannot drop its captured
        // resources before the gate is released and the connection is fenced.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let waker = std::task::Waker::noop();
            let mut context = Context::from_waker(waker);
            self.future.as_mut().poll(&mut context)
        }));
        drop(admission);
        match outcome {
            Ok(result) => self.observe(result),
            Err(payload) => {
                self.lifecycle.fail();
                std::panic::resume_unwind(payload)
            }
        }
    }

    /// Record a terminal result before any send-owned resources can be dropped.
    fn observe<T>(&mut self, result: Poll<Result<T>>) -> Poll<Result<T>> {
        if let Poll::Ready(outcome) = &result {
            if outcome.is_err() {
                self.lifecycle.fail();
            }
            self.completed = true;
        }
        result
    }
}

impl<F, T> Future for OwnedSend<F>
where F: Future<Output = Result<T>>
{
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // Catch before unwinding the wrapper. The future remains pinned and
        // owned here until failure has fenced the generation.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().poll(context)
        }));
        match outcome {
            Ok(result) => self.observe(result),
            Err(payload) => {
                self.lifecycle.fail();
                std::panic::resume_unwind(payload)
            }
        }
    }
}

impl<F: Future> Drop for OwnedSend<F> {
    fn drop(&mut self) {
        if !self.completed {
            self.lifecycle.fail();
        }
    }
}
