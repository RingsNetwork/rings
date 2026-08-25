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

use super::NATIVE_SEND_COMPLETION_TIMEOUT;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::SendAcceptance;
use crate::error::Error;
use crate::error::Result;
use crate::sync_utils::lock_recover;

/// Connection-generation gate shared by every native data channel.
#[derive(Clone)]
pub(super) struct NativeRetirementFence {
    connection_state: ConnectionStateCell,
    cancel_token: CancellationToken,
    retired: Arc<Mutex<bool>>,
    #[cfg(test)]
    waiting_retirements: Arc<std::sync::atomic::AtomicUsize>,
}

/// Held only across final permit claim and the send primitive's first poll.
pub(super) struct NativeSendAdmission<'a> {
    _retired: MutexGuard<'a, bool>,
}

impl NativeRetirementFence {
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
        if *retired {
            return None;
        }
        Some(NativeSendAdmission { _retired: retired })
    }

    fn finish_retirement(&self, retired: &mut bool) {
        *retired = true;
        self.connection_state.close();
        self.cancel_token.cancel();
    }

    pub(super) fn request(&self) {
        let mut retired = lock_recover(&self.retired);
        self.finish_retirement(&mut retired);
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

pub(super) struct RetirementFenceGuard {
    fence: NativeRetirementFence,
    acceptance: Option<SendAcceptance>,
    authority: Option<()>,
}

impl RetirementFenceGuard {
    pub(super) fn new(fence: NativeRetirementFence) -> Self {
        Self {
            fence,
            acceptance: None,
            authority: Some(()),
        }
    }

    pub(super) fn once_irrevocable(
        fence: NativeRetirementFence,
        acceptance: SendAcceptance,
    ) -> Self {
        Self {
            fence,
            acceptance: Some(acceptance),
            authority: Some(()),
        }
    }

    pub(super) fn retire(&mut self) {
        if self.authority.take().is_some()
            && self
                .acceptance
                .as_ref()
                .is_none_or(SendAcceptance::is_irrevocable)
        {
            self.fence.request();
        }
    }

    pub(super) fn disarm(&mut self) {
        self.authority.take();
    }

    pub(super) fn fence(&self) -> &NativeRetirementFence {
        &self.fence
    }
}

impl Drop for RetirementFenceGuard {
    fn drop(&mut self) {
        self.retire();
    }
}

pub(super) fn native_send_runtime() -> Result<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().map_err(|_| Error::NativeSendRuntimeUnavailable)
}

pub(super) async fn run_native_close_task(
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = Result<()>> + Send + 'static,
) -> Result<()> {
    runtime
        .spawn(close)
        .await
        .map_err(Error::NativeConnectionCloseTask)?
}

pub(super) fn poll_once_while_guarded<F, G>(
    mut future: std::pin::Pin<&mut F>,
    guard: G,
) -> std::task::Poll<F::Output>
where
    F: Future + ?Sized,
{
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        future.as_mut().poll(&mut context)
    }));
    drop(guard);
    match outcome {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

pub(super) async fn run_irrevocable_send<T>(
    runtime: &tokio::runtime::Handle,
    retirement_fence: NativeRetirementFence,
    send: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    run_irrevocable_send_with_timeout(
        runtime,
        NATIVE_SEND_COMPLETION_TIMEOUT,
        retirement_fence,
        send,
    )
    .await
}

pub(super) async fn run_irrevocable_send_with_timeout<T>(
    runtime: &tokio::runtime::Handle,
    completion_timeout: Duration,
    retirement_fence: NativeRetirementFence,
    send: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    runtime
        .spawn(async move {
            let mut send = Box::pin(send);
            let mut retirement = RetirementFenceGuard::new(retirement_fence);
            tokio::select! {
                result = send.as_mut() => {
                    if result.is_ok() {
                        retirement.disarm();
                    } else {
                        retirement.retire();
                    }
                    result
                }
                _ = tokio::time::sleep(completion_timeout) => {
                    retirement.retire();
                    Err(Error::NativeSendCompletionTimeout {
                        timeout_ms: completion_timeout.as_millis(),
                    })
                }
            }
        })
        .await
        .map_err(Error::NativeSendTask)?
}

struct PhysicalRetirementGuard<F>
where F: Future<Output = Result<()>> + Send + 'static
{
    runtime: tokio::runtime::Handle,
    acceptance: SendAcceptance,
    retirement: Option<F>,
}

impl<F> PhysicalRetirementGuard<F>
where F: Future<Output = Result<()>> + Send + 'static
{
    fn new(runtime: tokio::runtime::Handle, acceptance: SendAcceptance, retirement: F) -> Self {
        Self {
            runtime,
            acceptance,
            retirement: Some(retirement),
        }
    }

    fn disarm(&mut self) {
        self.retirement = None;
    }

    async fn retire(&mut self) -> Result<()> {
        match self.retirement.take() {
            Some(retirement) => retirement.await,
            None => Ok(()),
        }
    }
}

impl<F> Drop for PhysicalRetirementGuard<F>
where F: Future<Output = Result<()>> + Send + 'static
{
    fn drop(&mut self) {
        if self.acceptance.is_irrevocable() {
            if let Some(retirement) = self.retirement.take() {
                self.runtime.spawn(async move {
                    if let Err(error) = retirement.await {
                        tracing::warn!(%error, "failed to retire abandoned native send");
                    }
                });
            }
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

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

pub(super) async fn run_send_with_retirement<T>(
    runtime: &tokio::runtime::Handle,
    acceptance: SendAcceptance,
    retirement_fence: NativeRetirementFence,
    send: impl Future<Output = Result<T>> + Send + 'static,
    retirement: impl Future<Output = Result<()>> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let mut physical_retirement =
        PhysicalRetirementGuard::new(runtime.clone(), acceptance.clone(), retirement);
    let mut fence_retirement =
        RetirementFenceGuard::once_irrevocable(retirement_fence, acceptance.clone());
    let result = match catch_future_unwind(send).await {
        Ok(result) => result,
        Err(payload) => Err(Error::NativeSendPanic(panic_message(payload.as_ref()))),
    };
    if result.is_err() && acceptance.is_irrevocable() {
        fence_retirement.retire();
        if let Err(error) = physical_retirement.retire().await {
            tracing::warn!(%error, "failed to retire connection after irrevocable send error");
        }
    } else {
        fence_retirement.disarm();
        physical_retirement.disarm();
    }
    result
}
