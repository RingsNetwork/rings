//! Executor-neutral one-shot close actor; only the adapters choose mailbox and scheduler.

use std::future::Future;

use super::model::close_step;
use super::model::CloseEffect;
use super::model::CloseEvent;
use super::model::CloseState;
use crate::error::Result;

/// Unique actor state owner. Publication copies a value, never mutable authority.
struct Reporter<P: Fn(CloseState)> {
    /// Current reducer state, exclusively advanced by this owner.
    state: CloseState,
    /// Adapter-specific snapshot publication, without a runtime bound.
    publish: P,
}

impl<P: Fn(CloseState)> Reporter<P> {
    /// Commit the pure transition before publishing or interpreting its IO effect.
    fn apply(&mut self, event: CloseEvent) -> CloseEffect {
        let (state, effect) = close_step(self.state, event);
        self.state = state;
        (self.publish)(state);
        effect
    }
}

impl<P: Fn(CloseState)> Drop for Reporter<P> {
    fn drop(&mut self) {
        self.apply(CloseEvent::RuntimeStopped);
    }
}

/// Construct the actor before spawning, covering destruction before the first poll.
///
/// Pre: inbox resolves to Fenced only after synchronous generation retirement,
/// or ObserversGone when no request can arrive. Post: close is polled at most
/// once per actor, only after Fenced. Scheduling and Send bounds belong to callers.
/// Succeeded means the close adapter fulfilled its contract, not remote acknowledgement.
pub(crate) fn run(
    inbox: impl Future<Output = CloseEvent>,
    close: impl Future<Output = Result<()>>,
    publish: impl Fn(CloseState),
) -> impl Future<Output = ()> {
    // Construct outside the async body so even an unpolled dropped task reports interruption.
    let mut reporter = Reporter {
        state: CloseState::Idle,
        publish,
    };
    async move {
        match reporter.apply(inbox.await) {
            CloseEffect::StartClose => {
                let completion = match close.await {
                    Ok(()) => CloseEvent::CloseSucceeded,
                    Err(error) => {
                        tracing::warn!(%error, "physical close adapter failed");
                        CloseEvent::CloseFailed
                    }
                };
                reporter.apply(completion);
            }
            CloseEffect::None | CloseEffect::Publish(_) => (),
        }
    }
}
