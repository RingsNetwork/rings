//! Shared polling and destruction boundary for native and browser sends.

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use super::lifecycle::FailureObserver;
use super::model::observation_step;
use super::model::Observation;
use super::model::ObservationEffect;
use super::model::ObservationState;
use crate::error::Result;

/// Own a pinned future and interpret the pure observation machine before destruction.
/// Invariant: report_failure precedes field Drop; no close future lives in this owner.
pub(crate) struct OwnedSend<F: Future, L: FailureObserver> {
    /// Resource captures stay here through the failure-reporting boundary.
    future: Pin<Box<F>>,
    /// Message address plus the synchronous fence adapter, shared without close authority.
    lifecycle: L,
    /// Explicit local observation state, advanced only by observation_step.
    state: ObservationState,
}

impl<F: Future, L: FailureObserver> OwnedSend<F, L> {
    /// Install the owner before first poll or permit claim can occur.
    pub(crate) fn new(future: F, lifecycle: L) -> Self {
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

    /// First queue poll requires an open-generation lease issued by AdmissionGate.
    #[cfg(feature = "native-webrtc")]
    pub(crate) fn poll_admitted<T>(
        &mut self,
        admission: super::gate::FirstPollLease<'_>,
    ) -> Poll<Result<T>>
    where
        F: Future<Output = Result<T>>,
    {
        let mut context = Context::from_waker(std::task::Waker::noop());
        self.poll_guarded(&mut context, admission)
    }

    /// Common polling interpreter, retaining captures through catch and gate release.
    /// Pre: guard is the admission lease for first poll, unit for continuation polls.
    /// Post: guard is dropped before reporting any failure, preventing recursive gate lock.
    pub(super) fn poll_guarded<G, T>(
        &mut self,
        context: &mut Context<'_>,
        guard: G,
    ) -> Poll<Result<T>>
    where
        F: Future<Output = Result<T>>,
    {
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

impl<F, T, L: FailureObserver> Future for OwnedSend<F, L>
where F: Future<Output = Result<T>>
{
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_guarded(context, ())
    }
}

impl<F: Future, L: FailureObserver> Drop for OwnedSend<F, L> {
    fn drop(&mut self) {
        self.observe(Observation::Abandoned);
    }
}
