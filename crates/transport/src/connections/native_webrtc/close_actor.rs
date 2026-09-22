//! Tokio mailbox, scheduling and observation adapters for the shared close actor.

use std::future::Future;

use tokio::sync::mpsc;
use tokio::sync::watch;

use super::send_runtime::FencedCommand;
use crate::core::send::actor;
use crate::core::send::model::CloseEvent;
use crate::core::send::model::CloseOutcome;
use crate::core::send::model::CloseState;
use crate::error::Result;

/// Transfer the close future to the common actor and schedule it on Tokio.
pub(super) fn spawn(
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = Result<()>> + Send + 'static,
) -> (mpsc::Sender<FencedCommand>, watch::Receiver<CloseState>) {
    let (sender, mut mailbox) = mpsc::channel(1);
    let (status, receiver) = watch::channel(CloseState::Idle);
    let inbox = async move {
        mailbox
            .recv()
            .await
            .map_or(CloseEvent::ObserversGone, |_| CloseEvent::Fenced)
    };
    runtime.spawn(actor::run(inbox, close, move |state| {
        status.send_replace(state);
    }));
    (sender, receiver)
}

/// Await a terminal snapshot; executor disappearance never implies successful close.
pub(super) async fn outcome(mut status: watch::Receiver<CloseState>) -> CloseOutcome {
    match status.wait_for(|state| state.outcome().is_some()).await {
        Ok(state) => state.outcome().unwrap_or(CloseOutcome::Interrupted),
        Err(_) => CloseOutcome::Interrupted,
    }
}
