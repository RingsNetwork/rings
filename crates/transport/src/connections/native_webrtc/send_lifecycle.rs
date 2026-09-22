//! Native effects for the shared send lifecycle and polling owner.

use std::future::Future;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::close_actor;
use super::send_runtime::FencedCommand;
use super::send_runtime::NativeRetirementFence;
use super::send_runtime::NativeSendAdmission;
use crate::core::send::lifecycle;
use crate::core::send::lifecycle::Retirement;
use crate::core::send::model::CloseState;
use crate::core::send::owner;
use crate::core::transport::SendAcceptance;
use crate::error::Result;

/// Native address capabilities; close state and future belong to the shared actor.
pub(super) struct NativeRetirement {
    /// Cross-thread generation admission fence.
    fence: NativeRetirementFence,
    /// Bounded, coalescing actor address.
    mailbox: mpsc::Sender<FencedCommand>,
    /// Monotone indication that this send requested retirement.
    requested: CancellationToken,
    /// Read-only actor snapshots for cleanup waiters.
    status: watch::Receiver<CloseState>,
}

/// Specialization of the common admission/failure policy for native effects.
pub(super) type SendLifecycle = lifecycle::SendLifecycle<NativeRetirement>;
/// Specialization of the common resource owner for a thread-safe observer address.
pub(super) type OwnedSend<F> = owner::OwnedSend<F, Arc<SendLifecycle>>;

impl Retirement for NativeRetirement {
    type Command = FencedCommand;
    fn fence(&self) -> Self::Command {
        self.fence.commit()
    }
    fn notify(&self, command: Self::Command) {
        self.requested.cancel();
        let _delivery = self.mailbox.try_send(command);
    }
}

impl SendLifecycle {
    /// Create the close actor before exposing its shared observation address.
    pub(super) fn new(
        runtime: tokio::runtime::Handle,
        acceptance: SendAcceptance,
        fence: NativeRetirementFence,
        close: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Arc<Self> {
        let (mailbox, status) = close_actor::spawn(&runtime, close);
        Arc::new(Self::with_adapter(acceptance, NativeRetirement {
            fence,
            mailbox,
            requested: CancellationToken::new(),
            status,
        }))
    }

    /// Wait only for requested cleanup; the actor remains independent of its waiter.
    pub(super) async fn wait_for_cleanup(&self) {
        if self.adapter.requested.is_cancelled() {
            let _outcome = close_actor::outcome(self.adapter.status.clone()).await;
        }
    }

    /// Expose terminal results for conformance tests without sharing mutable actor state.
    #[cfg(test)]
    pub(super) async fn outcome(&self) -> crate::core::send::model::CloseOutcome {
        close_actor::outcome(self.adapter.status.clone()).await
    }
}

impl<F: Future> OwnedSend<F> {
    /// Native first polling requires the actual generation lease.
    pub(super) fn poll_admitted<T>(
        &mut self,
        admission: NativeSendAdmission<'_>,
    ) -> Poll<Result<T>>
    where
        F: Future<Output = Result<T>>,
    {
        let mut context = Context::from_waker(std::task::Waker::noop());
        self.poll_guarded(&mut context, admission)
    }
}
