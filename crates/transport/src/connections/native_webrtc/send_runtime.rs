//! Native send execution and connection-retirement boundaries.
//!
//! The backend polls a send primitive once while holding the retirement fence;
//! after irrevocable admission, a bounded continuation owns completion. Panic,
//! timeout, or abandoned continuation retires the connection generation before
//! cleanup, so callers never treat an uncertain physical write as cancellable.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::send_lifecycle::OwnedSend;
use super::send_lifecycle::SendLifecycle;
use super::NATIVE_SEND_COMPLETION_TIMEOUT;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::SendAcceptance;
use crate::error::Error;
use crate::error::Result;
use crate::sync_utils::lock_recover;

/// Connection-generation gate shared by every native data channel.
/// Cloning shares the same gate and cancellation signal; it does not create a generation.
#[derive(Clone)]
pub(super) struct NativeRetirementFence {
    /// Public logical connection state, closed by retirement.
    connection_state: ConnectionStateCell,
    /// Wakeup signal for connection-owned work when retirement is committed.
    cancel_token: CancellationToken,
    /// Serializes the first physical poll against generation retirement.
    retired: Arc<Mutex<bool>>,
    #[cfg(test)]
    /// Test witness that a competing retirement reached the admission gate.
    waiting_retirements: Arc<std::sync::atomic::AtomicUsize>,
}

/// Non-forgeable evidence delivered only after the synchronous generation fence.
/// No Clone/Copy: each delivered command is produced by an actual fence operation.
pub(super) struct FencedCommand {
    /// Private construction restricts issuance to NativeRetirementFence::commit.
    _sealed: (),
}

/// Held only across final permit claim and the send primitive's first poll.
pub(super) struct NativeSendAdmission<'a> {
    /// Lease excluding retirement until the guarded first poll returns.
    _retired: MutexGuard<'a, bool>,
}

impl NativeRetirementFence {
    /// Bind one shared gate to the generation state and its cancellation signal.
    pub(super) fn new(
        connection_state: ConnectionStateCell,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            connection_state,
            cancel_token,
            retired: Arc::new(Mutex::new(false)),
            #[cfg(test)]
            waiting_retirements: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Linearize final permit admission with connection-wide retirement.
    pub(super) fn try_send_admission(&self) -> Option<NativeSendAdmission<'_>> {
        let retired = lock_recover(&self.retired);
        match *retired {
            true => None,
            false => Some(NativeSendAdmission { _retired: retired }),
        }
    }

    /// Publish logical closure while holding exclusive generation admission.
    fn finish_retirement(&self, retired: &mut bool) {
        *retired = true;
        self.connection_state.close();
        self.cancel_token.cancel();
    }

    /// Close admission synchronously before any asynchronous physical cleanup.
    pub(super) fn request(&self) {
        let mut retired = lock_recover(&self.retired);
        self.finish_retirement(&mut retired);
    }

    /// Commit fencing before issuing the actor command capability.
    pub(super) fn commit(&self) -> FencedCommand {
        self.request();
        FencedCommand { _sealed: () }
    }

    #[cfg(test)]
    pub(super) fn request_with_observer_for_test(&self, before_gate: impl FnOnce()) {
        self.waiting_retirements
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        before_gate();
        let mut retired = lock_recover(&self.retired);
        self.waiting_retirements
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.finish_retirement(&mut retired);
    }

    #[cfg(test)]
    pub(super) fn waiting_retirements_for_test(&self) -> usize {
        self.waiting_retirements
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Require a live executor handle at the native send boundary.
pub(super) fn native_send_runtime() -> Result<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().map_err(|_| Error::NativeSendRuntimeUnavailable)
}

/// Detach physical close ownership from the lifetime of its join waiter.
pub(super) async fn run_native_close_task(
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = Result<()>> + Send + 'static,
) -> Result<()> {
    runtime
        .spawn(close)
        .await
        .map_err(Error::NativeConnectionCloseTask)
        .and_then(std::convert::identity)
}

/// Move the already-admitted resource owner into a bounded continuation.
pub(super) async fn run_irrevocable_send<T>(
    runtime: &tokio::runtime::Handle,
    send: OwnedSend<impl Future<Output = Result<T>> + Send + 'static>,
) -> Result<T>
where
    T: Send + 'static,
{
    run_irrevocable_send_with_timeout(runtime, NATIVE_SEND_COMPLETION_TIMEOUT, send).await
}

/// Execute the owned primitive with a cooperative timeout and typed join errors.
pub(super) async fn run_irrevocable_send_with_timeout<T>(
    runtime: &tokio::runtime::Handle,
    completion_timeout: Duration,
    send: OwnedSend<impl Future<Output = Result<T>> + Send + 'static>,
) -> Result<T>
where
    T: Send + 'static,
{
    runtime
        .spawn(async move {
            // The same resource owner moves into the task. On timeout, panic,
            // or runtime cancellation its Drop fences before releasing captures.
            let mut send = send;
            tokio::select! {
                result = &mut send => result,
                _ = tokio::time::sleep(completion_timeout) => {
                    drop(send);
                    Err(Error::NativeSendCompletionTimeout {
                        timeout_ms: completion_timeout.as_millis(),
                    })
                }
            }
        })
        .await
        .map_err(Error::NativeSendTask)
        .and_then(std::convert::identity)
}

/// Preserve a human-readable panic payload without assuming its concrete type.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Convert caller-owned polling panics without letting unwind bypass the owner.
async fn catch_future_unwind<F>(future: F) -> std::thread::Result<F::Output>
where F: Future {
    let mut future = Box::pin(future);
    std::future::poll_fn(move |context| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            future.as_mut().poll(context)
        })) {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => std::task::Poll::Ready(Err(payload)),
        }
    })
    .await
}

/// Install the single retirement authority and return the original send outcome.
/// Error waiters await independently-owned cleanup; cancelling that wait cannot stop it.
pub(super) async fn run_send_with_retirement<T, F>(
    runtime: &tokio::runtime::Handle,
    acceptance: SendAcceptance,
    retirement_fence: NativeRetirementFence,
    send: impl FnOnce(Arc<SendLifecycle>) -> F,
    retirement: impl Future<Output = Result<()>> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    // The actor alone owns cleanup; the caller and continuation share its address.
    // Pre: the factory only constructs the unpolled future, without performing IO.
    let lifecycle = SendLifecycle::new(runtime.clone(), acceptance, retirement_fence, retirement);
    let send = OwnedSend::new(send(Arc::clone(&lifecycle)), Arc::clone(&lifecycle));
    let result = match catch_future_unwind(send).await {
        Ok(result) => result,
        Err(payload) => Err(Error::NativeSendPanic(panic_message(payload.as_ref()))),
    };
    match result {
        Err(error) => {
            lifecycle.wait_for_cleanup().await;
            Err(error)
        }
        Ok(value) => Ok(value),
    }
}
