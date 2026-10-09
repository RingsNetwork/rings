//! A transfer handed to its peer's outbound worker, and what its completion is awaited and
//! reported against, apart from the sender that queued it.

use std::time::Duration;

use futures::future::FusedFuture;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;

use super::await_bounded_cleanup;
use super::resolve_scheduler_loss;
use super::DetachedAdmissionOnDrop;
use super::OutboundSendLog;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::swarm::observer::LookupOutcome;
use crate::swarm::observer::MessageActivity;
use crate::swarm::observer::MessageObservation;
use crate::swarm::observer::ObservationOutcome;
use crate::swarm::observer::SharedSwarmObserver;
use crate::swarm::transport::delivery::terminate_accepted_connection;
use crate::swarm::transport::delivery::SendCompletionOutcome;
use crate::swarm::transport::outbound::DetachedAdmissionCancel;
use crate::swarm::transport::outbound::OutboundCompletion;
use crate::swarm::transport::timeouts::OUTBOUND_PAYLOAD_CLEANUP_GRACE;
use crate::swarm::transport::AdmittedConnection;

/// What a submitted transfer's completion is: published later by the worker, or settled at
/// submission because the worker was gone.
pub(super) enum TransferReceipt {
    /// The worker publishes the completion.
    Pending(futures::channel::oneshot::Receiver<Result<SendCompletionOutcome>>),
    /// The submission itself settled the transfer.
    Settled(Result<SendCompletionOutcome>),
}

/// A transfer in its peer's outbound queue, with what its completion is observed and logged
/// against. It owns everything it reports to, so its completion can be awaited apart from the
/// sender that queued it.
pub(super) struct SubmittedTransfer {
    pub(super) local: Did,
    pub(super) observer: SharedSwarmObserver,
    pub(super) admitted: AdmittedConnection,
    pub(super) receipt: TransferReceipt,
    pub(super) successor_lookup: Option<LookupCorrelation>,
    pub(super) log: OutboundSendLog,
}

impl SubmittedTransfer {
    /// Await the transfer's completion, record it with the observer, and log its acceptance.
    pub(super) async fn complete(self) -> Result<SendCompletionOutcome> {
        let Self {
            local,
            observer,
            admitted,
            receipt,
            successor_lookup,
            log,
        } = self;
        let result = match receipt {
            TransferReceipt::Settled(result) => result,
            TransferReceipt::Pending(receiver) => match receiver.await {
                Ok(result) => result,
                Err(_) => resolve_scheduler_loss(
                    &admitted,
                    Error::ChannelRecvMessageFailed("outbound scheduler stopped".into()),
                ),
            },
        };
        let observation_outcome = match &result {
            Ok(SendCompletionOutcome::Succeeded) => ObservationOutcome::Succeeded,
            Ok(SendCompletionOutcome::Cancelled) | Err(_) => ObservationOutcome::Failed,
        };
        let activity = if log.origin == local {
            MessageActivity::Sent
        } else {
            MessageActivity::Forwarded
        };
        observer.observe_message(MessageObservation {
            activity,
            category: log.category,
            message_class: log.message_kind,
            outcome: observation_outcome,
        });
        if let Some(correlation) = successor_lookup {
            if matches!(observation_outcome, ObservationOutcome::Failed) {
                observer.lookup_finished(LookupKind::Successor, correlation, LookupOutcome::Failed);
            }
        }

        let outcome = result?;

        tracing::debug!(
            local = %local,
            next_hop = %log.next_hop,
            destination = %log.destination,
            relay_destination = %log.relay_destination,
            tx_id = %log.tx_id,
            message_kind = log.message_kind,
            tracked = matches!(log.completion, OutboundCompletion::Tracked),
            succeeded = matches!(outcome, SendCompletionOutcome::Succeeded),
            "send payload accepted"
        );
        Ok(outcome)
    }
}

/// A detached payload in its peer's outbound queue whose first frame is not yet admitted, with
/// the guard that cancels it if it is abandoned before admission.
pub(super) struct EnqueuedDetached {
    pub(super) peer: Did,
    pub(super) timeout_budget: Duration,
    pub(super) submitted: SubmittedTransfer,
    pub(super) cancel_on_drop: DetachedAdmissionOnDrop,
}

impl EnqueuedDetached {
    /// The timeout of this payload's first-frame admission.
    fn timeout_error(&self) -> Error {
        Error::OutboundFirstFrameAdmissionTimeout {
            peer: self.peer,
            timeout_ms: self.timeout_budget.as_millis(),
        }
    }

    /// Await the first frame's admission until `deadline`; past it the transfer is cancelled,
    /// and a transfer that does not settle within the cleanup grace retires its connection.
    pub(super) async fn await_admission(
        self,
        deadline: impl FusedFuture<Output = ()> + Unpin,
    ) -> Result<SendCompletionOutcome> {
        let timeout_error = self.timeout_error();
        let Self {
            peer,
            submitted,
            mut cancel_on_drop,
            ..
        } = self;
        let cleanup_connection = submitted.admitted.clone();
        let send = submitted.complete().fuse();
        pin_mut!(send, deadline);
        let result = select! {
            result = send => result,
            _ = deadline => {
                let cancellation = cancel_on_drop.cancel();
                match await_bounded_cleanup(send, OUTBOUND_PAYLOAD_CLEANUP_GRACE).await {
                    Some(result) if cancellation == DetachedAdmissionCancel::MustAwait => result,
                    Some(result) => {
                        match result? {
                            SendCompletionOutcome::Succeeded => {
                                Err(Error::CancelledDetachedAdmissionPublishedSuccess)
                            }
                            SendCompletionOutcome::Cancelled => Err(timeout_error),
                        }
                    }
                    None => {
                        terminate_accepted_connection(
                            &cleanup_connection,
                            "detached_payload_cleanup_timeout",
                        )
                        .await;
                        Err(Error::DetachedPayloadCleanupTimeout {
                            peer,
                            timeout_ms: OUTBOUND_PAYLOAD_CLEANUP_GRACE.as_millis(),
                        })
                    }
                }
            },
        };
        cancel_on_drop.disarm();
        result
    }
}
