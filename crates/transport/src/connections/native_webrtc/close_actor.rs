//! One-shot actor and effect interpreter for native physical close.
//!
//! The actor exclusively owns the close future and its lifecycle state. Senders
//! can only enqueue a fenced command; they cannot mutate that state or poll close.
//! The capacity-one mailbox coalesces duplicate requests without an unbounded queue.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio::sync::watch;

use super::send_model::close_step;
use super::send_model::CloseEffect;
use super::send_model::CloseEvent;
use super::send_model::CloseOutcome;
use super::send_model::CloseState;
use super::send_runtime::FencedCommand;
use crate::error::Result;

/// Watch publication and actor-local state have exactly one writer, including Drop.
struct Reporter {
    /// Reducer state, never shared through a mutex or mutable handle.
    state: CloseState,
    /// Read-only snapshots for observation clients; only this actor owns the sender.
    status: watch::Sender<CloseState>,
}

impl Reporter {
    /// Interpret a pure transition by committing state and publishing its snapshot.
    fn apply(&mut self, event: CloseEvent) -> CloseEffect {
        // `transition` is the sole specification used by production and exploration.
        let (state, effect) = close_step(self.state, event);
        self.state = state;
        self.status.send_replace(state);
        effect
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        // Pre: no other writer can race this destructor. Post: watchers receive
        // Interrupted unless the reducer had already committed a terminal result.
        self.apply(CloseEvent::RuntimeStopped);
    }
}

/// Actor-owned capabilities. No field is cloned when the actor starts closing.
struct CloseActor {
    /// Only the actor can dequeue the coalesced retirement request.
    mailbox: mpsc::Receiver<FencedCommand>,
    /// Unique physical-close future, unpolled until the reducer emits StartClose.
    close: Pin<Box<dyn Future<Output = Result<()>> + Send>>,
    /// State writer constructed before spawn, including unpolled shutdown coverage.
    reporter: Reporter,
}

impl CloseActor {
    /// One irreversible command is the entire actor protocol; no mutable state escapes.
    async fn run(self) {
        // Destructure once to give mailbox, close IO, and publication separate owners.
        let Self {
            mut mailbox,
            close,
            mut reporter,
        } = self;
        // Receiver closure is an explicit protocol input, not a fake close success.
        let event = mailbox
            .recv()
            .await
            .map_or(CloseEvent::ObserversGone, |_| CloseEvent::Fenced);
        match reporter.apply(event) {
            CloseEffect::StartClose => {
                // All later commands are duplicates. Dropping the receiver bounds
                // retained requests while the actor awaits its sole IO operation.
                drop(mailbox);
                let completion = match close.await {
                    Ok(()) => CloseEvent::CloseSucceeded,
                    Err(error) => {
                        tracing::warn!(%error, "native physical close failed");
                        CloseEvent::CloseFailed
                    }
                };
                reporter.apply(completion);
            }
            CloseEffect::None | CloseEffect::Publish(_) => {}
        }
    }
}

/// Spawn a single state owner and return only its message address and read-only status.
///
/// Post: no polling of `close` precedes receipt of a FencedCommand. Runtime shutdown
/// drops the preconstructed Reporter even when the actor was never first-polled.
pub(super) fn spawn(
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = Result<()>> + Send + 'static,
) -> (mpsc::Sender<FencedCommand>, watch::Receiver<CloseState>) {
    // Capacity one is sufficient: retirement is monotone and requests are equivalent.
    let (sender, mailbox) = mpsc::channel(1);
    // Watchers receive immutable copies; Reporter is the only writer.
    let (status, receiver) = watch::channel(CloseState::Idle);
    // Build the Drop boundary outside the async body for unpolled cancellation.
    let actor = CloseActor {
        mailbox,
        close: Box::pin(close),
        reporter: Reporter {
            state: CloseState::Idle,
            status,
        },
    };
    runtime.spawn(actor.run());
    (sender, receiver)
}

/// Wait on actor snapshots; a dropped executor is explicitly Interrupted, never success.
pub(super) async fn outcome(mut status: watch::Receiver<CloseState>) -> CloseOutcome {
    match status.wait_for(|state| state.outcome().is_some()).await {
        Ok(state) => state.outcome().unwrap_or(CloseOutcome::Interrupted),
        Err(_) => CloseOutcome::Interrupted,
    }
}
