//! Shared failure interpretation, parameterized only by platform effects.

use std::rc::Rc;
use std::sync::Arc;

use super::model::failure_effect;
use super::model::FailureEffect;
use crate::core::transport::SendAcceptance;

/// Platform retirement effects; fencing must complete synchronously before notification.
pub(crate) trait Retirement {
    /// A platform capability witnessing committed logical retirement.
    type Command;
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
    pub(crate) adapter: B,
}

impl<B: Retirement> SendLifecycle<B> {
    /// Bind an admission observer to an already-created close actor address.
    pub(crate) fn with_adapter(acceptance: SendAcceptance, adapter: B) -> Self {
        Self {
            acceptance,
            adapter,
        }
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
