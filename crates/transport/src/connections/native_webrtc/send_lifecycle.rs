//! Synchronous failure boundary and resource-owner adapter around the close actor.
//!
//! State decisions live in send_model. This module interprets only synchronous
//! fencing, bounded mailbox delivery, polling and destruction. Physical IO and
//! close-state mutation belong exclusively to close_actor.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::close_actor;
use super::send_model::failure_effect;
use super::send_model::observation_step;
use super::send_model::CloseState;
use super::send_model::FailureEffect;
use super::send_model::Observation;
use super::send_model::ObservationEffect;
use super::send_model::ObservationState;
use super::send_runtime::FencedCommand;
use super::send_runtime::NativeRetirementFence;
use super::send_runtime::NativeSendAdmission;
use crate::core::transport::SendAcceptance;
use crate::error::Result;

/// Cloneable observation rights to one actor, not shared mutable close ownership.
/// Arc clones preserve identity: all callers address the same bounded mailbox.
pub(super) struct SendLifecycle {
    /// Coherent permit snapshot source; the original atomic admission machine owns it.
    acceptance: SendAcceptance,
    /// Synchronous generation fence needed before Drop can release resources.
    fence: NativeRetirementFence,
    /// Capacity-one actor address; duplicate failure commands coalesce.
    mailbox: mpsc::Sender<FencedCommand>,
    /// Monotone evidence that a failure observer committed fencing for this send.
    requested: CancellationToken,
    /// Read-only actor snapshots; no sender can mutate actor-local state.
    status: watch::Receiver<CloseState>,
}

impl SendLifecycle {
    /// Create the actor before constructing the send, transferring close ownership once.
    pub(super) fn new(
        runtime: tokio::runtime::Handle,
        acceptance: SendAcceptance,
        fence: NativeRetirementFence,
        close: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Arc<Self> {
        // The actor owns all cleanup resources; this handle owns only observation rights.
        let (mailbox, status) = close_actor::spawn(&runtime, close);
        Arc::new(Self {
            acceptance,
            fence,
            mailbox,
            requested: CancellationToken::new(),
            status,
        })
    }

    /// Interpret the pure failure decision at the destruction-critical boundary.
    ///
    /// Pre: no admission lease is held by this thread. Post: an irrevocable failure
    /// fences synchronously before any message is delivered or resource is released.
    /// A full mailbox means the equivalent command is already queued; a closed
    /// mailbox means the actor is closing/terminal. Neither permits reopening.
    pub(super) fn fail(&self) {
        match failure_effect(self.acceptance.phase()) {
            FailureEffect::Ignore => (),
            FailureEffect::FenceThenNotify => {
                // Construction of this non-forgeable command witnesses a completed fence.
                let command = self.fence.commit();
                self.requested.cancel();
                // Delivery is nonblocking. Capacity one coalesces duplicate requests.
                let _delivery = self.mailbox.try_send(command);
            }
        }
    }

    /// Wait only after fencing was requested; never transfer cleanup ownership to a waiter.
    pub(super) async fn wait_for_cleanup(&self) {
        if self.requested.is_cancelled() {
            let _outcome = close_actor::outcome(self.status.clone()).await;
        }
    }

    /// Observe the actor's explicit terminal result for production-shell conformance tests.
    #[cfg(test)]
    pub(super) async fn outcome(&self) -> super::send_model::CloseOutcome {
        close_actor::outcome(self.status.clone()).await
    }
}

/// Own a pinned future and interpret the pure observation machine before destruction.
/// Invariant: report_failure precedes field Drop; no close future lives in this owner.
pub(super) struct OwnedSend<F: Future> {
    /// Resource captures stay here through the failure-reporting boundary.
    future: Pin<Box<F>>,
    /// Message address plus the synchronous fence adapter, shared without close authority.
    lifecycle: Arc<SendLifecycle>,
    /// Explicit local observation state, advanced only by observation_step.
    state: ObservationState,
}

impl<F: Future> OwnedSend<F> {
    /// Install the owner before first poll or permit claim can occur.
    pub(super) fn new(future: F, lifecycle: Arc<SendLifecycle>) -> Self {
        Self {
            future: Box::pin(future),
            lifecycle,
            state: ObservationState::Active,
        }
    }

    /// Interpret a reducer effect at the synchronous resource boundary.
    fn observe(&mut self, event: Observation) {
        // The state value is pure and replayable; only the effect handler touches the shell.
        let (state, effect) = observation_step(self.state, event);
        self.state = state;
        match effect {
            ObservationEffect::None => (),
            ObservationEffect::ReportFailure => self.lifecycle.fail(),
        }
    }

    /// First poll requires the actual generation lease, not an arbitrary generic guard.
    pub(super) fn poll_admitted<T>(
        &mut self,
        admission: NativeSendAdmission<'_>,
    ) -> Poll<Result<T>>
    where
        F: Future<Output = Result<T>>,
    {
        // The first poll only determines immediate readiness; detached polling installs its waker.
        let mut context = Context::from_waker(std::task::Waker::noop());
        self.poll_guarded(&mut context, admission)
    }

    /// Common polling interpreter, retaining captures through catch and gate release.
    /// Pre: guard is the admission lease for first poll, unit for continuation polls.
    /// Post: guard is dropped before reporting any failure, preventing recursive gate lock.
    fn poll_guarded<G, T>(&mut self, context: &mut Context<'_>, guard: G) -> Poll<Result<T>>
    where F: Future<Output = Result<T>> {
        // Catch without destroying the pinned future or its external resource owner.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().poll(context)
        }));
        drop(guard);
        match outcome {
            Ok(result) => {
                // Project payload-bearing Poll/Result into the small pure event alphabet.
                let event = match &result {
                    Poll::Pending => Observation::Pending,
                    Poll::Ready(Ok(_)) => Observation::Succeeded,
                    Poll::Ready(Err(_)) => Observation::Failed,
                };
                self.observe(event);
                result
            }
            Err(payload) => {
                self.observe(Observation::Panicked);
                std::panic::resume_unwind(payload)
            }
        }
    }
}

impl<F, T> Future for OwnedSend<F>
where F: Future<Output = Result<T>>
{
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_guarded(context, ())
    }
}

impl<F: Future> Drop for OwnedSend<F> {
    fn drop(&mut self) {
        self.observe(Observation::Abandoned);
    }
}
