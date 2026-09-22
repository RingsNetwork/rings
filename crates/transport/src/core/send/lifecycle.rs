//! Shared failure interpretation, parameterized only by platform effects.

use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

use super::model::failure_effect;
use super::model::CloseOutcome;
use super::model::FailureEffect;
use crate::core::transport::SendAcceptance;
use crate::error::Result;

/// Platform retirement effects; fencing must complete synchronously before notification.
pub(crate) trait Retirement {
    /// A platform capability witnessing committed logical retirement.
    type Command;
    /// Repeatable observation of terminal cleanup, independent of the waiter lifetime.
    type Completion: Future<Output = CloseOutcome>;
    /// Whether this send has synchronously fenced and requested cleanup.
    fn requested(&self) -> bool;
    /// Observe completion without acquiring ownership of the actor or its close future.
    fn completion(&self) -> Self::Completion;
    /// Post: the generation rejects new sends before this method returns.
    fn fence(&self) -> Self::Command;
    /// Deliver/coalesce the fenced command without transferring close ownership to its caller.
    fn notify(&self, command: Self::Command);
}

/// One send's observation address. The platform adapter never owns admission policy.
pub(crate) struct SendLifecycle<B> {
    /// Shared atomic authority for admission; reading does not duplicate the permit.
    acceptance: SendAcceptance,
    /// Fence and actor-address capabilities specific to the execution environment.
    adapter: B,
}

impl<B: Retirement> SendLifecycle<B> {
    /// Bind an admission observer to an already-created close actor address.
    pub(crate) fn with_adapter(acceptance: SendAcceptance, adapter: B) -> Self {
        Self {
            acceptance,
            adapter,
        }
    }

    /// Wait only for cleanup that this send requested. Dropping this waiter cannot cancel it.
    pub(crate) async fn wait_for_cleanup(&self) {
        if self.adapter.requested() {
            let _outcome = self.adapter.completion().await;
        }
    }

    /// Complete the common error contract after the owner has reported its result.
    /// Post: a failed, retired send returns only after observing a terminal cleanup outcome.
    /// Failed/Interrupted describe unsuccessful cleanup; neither implies physical success.
    pub(crate) async fn finish<T>(&self, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.wait_for_cleanup().await;
                Err(error)
            }
        }
    }

    /// Read a terminal outcome for cross-platform conformance tests.
    #[cfg(test)]
    pub(crate) async fn outcome(&self) -> CloseOutcome {
        self.adapter.completion().await
    }

    /// Interpret the shared failure algebra before any resource owner is destroyed.
    /// Pre: no first-poll admission lease is held by this thread.
    pub(crate) fn fail(&self) {
        match failure_effect(self.acceptance.phase()) {
            FailureEffect::Ignore => (),
            FailureEffect::FenceThenNotify => self.adapter.notify(self.adapter.fence()),
        }
    }
}

/// Observation capability consumed by the shared resource-owner shell.
/// Unpin applies only to the address, never to the primitive future it observes.
pub(crate) trait FailureObserver: Unpin {
    /// Synchronously interpret failure before releasing the owner's captures.
    fn fail(&self);
}

impl<B: Retirement> FailureObserver for Arc<SendLifecycle<B>> {
    fn fail(&self) {
        self.as_ref().fail();
    }
}

impl<B: Retirement> FailureObserver for Rc<SendLifecycle<B>> {
    fn fail(&self) {
        self.as_ref().fail();
    }
}
