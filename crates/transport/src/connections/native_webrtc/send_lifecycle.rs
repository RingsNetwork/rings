//! Native effects for the shared send lifecycle and polling owner.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::close_actor;
use super::send_runtime::FencedCommand;
use super::send_runtime::NativeRetirementFence;
use crate::core::send::lifecycle;
use crate::core::send::lifecycle::Retirement;
use crate::core::send::model::CloseOutcome;
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
    type Completion = Pin<Box<dyn Future<Output = CloseOutcome> + Send>>;
    fn requested(&self) -> bool {
        self.requested.is_cancelled()
    }
    fn completion(&self) -> Self::Completion {
        Box::pin(close_actor::outcome(self.status.clone()))
    }
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
}
