use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::SendPermit;
use rings_transport::delivery::DeliveryFuture;

use super::connection::await_bounded_connection_close;
use super::connection::DATA_CHANNEL_CLOSE_TIMEOUT;
use super::AdmittedConnection;
use super::PendingConnectionAttempt;
use super::TransportReadiness;
use crate::chunk::Chunk;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::StorageSyncDestination;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::session::SessionSk;
use crate::utils::sleep;

/// Production admission budget for one data-channel send.
///
/// Maintenance scheduling uses this bound to leave control-plane work a
/// deterministic window around storage repair.
pub(crate) const DATA_CHANNEL_SEND_ACCEPT_BUDGET: Duration = Duration::from_secs(5);

#[cfg(test)]
pub(super) const DATA_CHANNEL_SEND_ACCEPT_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(not(test))]
pub(super) const DATA_CHANNEL_SEND_ACCEPT_TIMEOUT: Duration = DATA_CHANNEL_SEND_ACCEPT_BUDGET;

#[cfg(test)]
const CHUNK_SEND_PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const CHUNK_SEND_PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[cfg(test)]
const DATA_CHANNEL_DELIVERY_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const DATA_CHANNEL_DELIVERY_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug)]
pub(super) enum ChunkSendCancelReason {
    TransferStopped,
    AdmissionRevoked(PendingConnectionAttempt),
    AdmissionCheckFailed(Error),
    TransportNotReady(TransportReadiness),
    RouteNoLongerPermitted,
    RouteCheckFailed(Error),
}

impl ChunkSendCancelReason {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::TransferStopped => "transfer_stopped",
            Self::AdmissionRevoked(_) => "admission_revoked",
            Self::AdmissionCheckFailed(_) => "admission_check_failed",
            Self::TransportNotReady(_) => "transport_not_ready",
            Self::RouteNoLongerPermitted => "route_no_longer_permitted",
            Self::RouteCheckFailed(_) => "route_check_failed",
        }
    }

    const fn transport_readiness(&self) -> Option<TransportReadiness> {
        match self {
            Self::TransportNotReady(readiness) => Some(*readiness),
            Self::TransferStopped
            | Self::AdmissionRevoked(_)
            | Self::AdmissionCheckFailed(_)
            | Self::RouteNoLongerPermitted
            | Self::RouteCheckFailed(_) => None,
        }
    }

    const fn check_error(&self) -> Option<&Error> {
        match self {
            Self::AdmissionCheckFailed(error) | Self::RouteCheckFailed(error) => Some(error),
            Self::TransferStopped
            | Self::AdmissionRevoked(_)
            | Self::TransportNotReady(_)
            | Self::RouteNoLongerPermitted => None,
        }
    }

    pub(super) const fn records_peer_failure(&self) -> bool {
        match self {
            Self::TransportNotReady(readiness) => readiness.is_terminal(),
            Self::TransferStopped
            | Self::AdmissionRevoked(_)
            | Self::AdmissionCheckFailed(_)
            | Self::RouteNoLongerPermitted
            | Self::RouteCheckFailed(_) => false,
        }
    }

    const fn attempt(&self) -> Option<PendingConnectionAttempt> {
        match self {
            Self::AdmissionRevoked(attempt) => Some(*attempt),
            Self::TransferStopped
            | Self::AdmissionCheckFailed(_)
            | Self::TransportNotReady(_)
            | Self::RouteNoLongerPermitted
            | Self::RouteCheckFailed(_) => None,
        }
    }

    /// Resolve cancellation before the first frame has been accepted.
    ///
    /// A vanished storage route is a normal cancellation before any bytes are
    /// accepted. A revoked generation, failed proof, or readiness failure
    /// remains an explicit error from the connection or route proof that
    /// admitted the send.
    pub(super) fn resolve_initial(self) -> Result<()> {
        match self {
            Self::TransferStopped => Ok(()),
            Self::AdmissionRevoked(attempt) => Err(Error::ConnectionAttemptSuperseded {
                peer: attempt.peer(),
                generation: attempt.generation(),
            }),
            Self::RouteNoLongerPermitted => Ok(()),
            Self::AdmissionCheckFailed(error) | Self::RouteCheckFailed(error) => Err(error),
            Self::TransportNotReady(readiness) => Err(Error::TransportNotReady {
                state: readiness.state(),
                data_channel_open: readiness.data_channel_open(),
            }),
        }
    }
}

pub(super) enum ChunkSendProgress<T> {
    Ready(T),
    Cancelled(ChunkSendCancelReason),
}

/// Combined caller and scheduler cancellation observed by every frame phase.
#[derive(Clone)]
pub(super) struct TransferStop {
    caller: StopToken,
    scheduler: StopToken,
}

impl TransferStop {
    pub(super) fn new(caller: StopToken) -> Self {
        Self {
            caller,
            scheduler: StopToken::never(),
        }
    }

    pub(super) fn bind_scheduler(&mut self, scheduler: StopToken) {
        self.scheduler = scheduler;
    }

    pub(super) fn should_stop(&self) -> bool {
        self.caller.should_stop() || self.scheduler.should_stop()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendCompletionOutcome {
    /// The boundary selected by the completion policy was reached: transport
    /// acceptance for detached sends, full delivery for tracked sends.
    Succeeded,
    /// A caller deadline, generation, readiness, route, or tracked-delivery condition was revoked.
    Cancelled,
}

/// Clone law: every clone contains the same immutable route proof and shared
/// DHT handle, so it evaluates the same route proposition at a given DHT state.
#[derive(Clone)]
pub(super) enum ChunkSendPermit {
    Always,
    StorageSyncRoute {
        dht: Arc<PeerRing>,
        destination: StorageSyncDestination,
        next_hop: Did,
    },
}

impl ChunkSendPermit {
    pub(super) fn for_message(dht: Arc<PeerRing>, next_hop: Did, message: &Message) -> Self {
        match message {
            Message::SyncEntriesWithSuccessor(msg) => Self::StorageSyncRoute {
                dht,
                destination: msg.destination,
                next_hop,
            },
            _ => Self::Always,
        }
    }

    fn check(&self) -> std::result::Result<(), ChunkSendCancelReason> {
        match self {
            Self::Always => Ok(()),
            Self::StorageSyncRoute {
                dht,
                destination,
                next_hop,
            } => match dht.storage_sync_route_still_permits(*destination, *next_hop) {
                Ok(true) => Ok(()),
                Ok(false) => Err(ChunkSendCancelReason::RouteNoLongerPermitted),
                Err(error) => Err(ChunkSendCancelReason::RouteCheckFailed(error)),
            },
        }
    }

    fn admits(&self, final_condition: impl FnOnce() -> bool) -> bool {
        match self {
            Self::Always => final_condition(),
            Self::StorageSyncRoute {
                dht,
                destination,
                next_hop,
            } => matches!(
                dht.with_permitted_storage_sync_route(*destination, *next_hop, final_condition),
                Ok(Some(true))
            ),
        }
    }
}

pub(super) async fn send_data_with_timeout(
    admitted: &AdmittedConnection,
    data: Bytes,
    permit: &ChunkSendPermit,
    stop: &TransferStop,
    did: Did,
    context: &'static str,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    let bytes = data.len();
    let admission = admitted.clone();
    let route = permit.clone();
    let transfer_stop = stop.clone();
    let send_permit = SendPermit::new(move || {
        admission
            .with_current_connection(|connection| {
                route.admits(|| {
                    !transfer_stop.should_stop() && connection.readiness().can_make_progress()
                })
            })
            .ok()
            .flatten()
            .unwrap_or(false)
    });
    let acceptance = send_permit.acceptance();
    let send = admitted.connection().send_data(data, send_permit).fuse();
    let timeout = sleep(DATA_CHANNEL_SEND_ACCEPT_TIMEOUT).fuse();
    pin_mut!(send, timeout);

    loop {
        if acceptance.is_irrevocable() {
            return complete_irrevocable_send(send.await, admitted).await;
        }
        if let Some(reason) = chunk_send_cancel_reason(admitted, permit, stop) {
            log_chunk_send_cancel(did, context, &reason);
            return ChunkSendProgress::Cancelled(reason);
        }
        let poll = sleep(CHUNK_SEND_PERMIT_POLL_INTERVAL).fuse();
        pin_mut!(poll);
        select! {
            result = send => {
                if matches!(
                    result,
                    Err(Error::Transport(rings_transport::error::Error::SendPermitRevoked))
                ) {
                    if let Some(reason) = chunk_send_cancel_reason(admitted, permit, stop) {
                        log_chunk_send_cancel(did, context, &reason);
                        return ChunkSendProgress::Cancelled(reason);
                    }
                }
                return if acceptance.is_irrevocable() {
                    complete_irrevocable_send(result, admitted).await
                } else {
                    ChunkSendProgress::Ready(result)
                };
            },
            _ = timeout => {
                if acceptance.is_irrevocable() {
                    return complete_irrevocable_send(send.await, admitted).await;
                }
                return ChunkSendProgress::Ready(Err(Error::DataChannelSendQueueTimeout {
                    peer: did,
                    timeout_ms: DATA_CHANNEL_SEND_ACCEPT_TIMEOUT.as_millis(),
                    bytes,
                    context,
                }));
            },
            _ = poll => {}
        }
    }
}

async fn complete_irrevocable_send(
    result: Result<DeliveryFuture>,
    admitted: &AdmittedConnection,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    if result.is_err() {
        if let Err(close_error) = terminate_accepted_connection(admitted).await {
            return ChunkSendProgress::Ready(Err(close_error));
        }
    }
    ChunkSendProgress::Ready(result)
}

pub(super) async fn record_measurement(
    measure: Option<MeasureImpl>,
    did: Did,
    counter: MeasureCounter,
) {
    if let Some(measure) = measure {
        measure.incr(did, counter).await;
    }
}

/// Frame one chunk into the bytes a data-channel send carries: wrap it in a `MessagePayload`
/// addressed to `did` and serialize it. Pure (the only failure is serialization).
pub(super) fn frame_chunk(session_sk: &SessionSk, did: Did, chunk: Chunk) -> Result<Bytes> {
    MessagePayload::new_send(Message::Chunk(chunk), session_sk, did, did)?.to_wire()
}

fn chunk_send_cancel_reason(
    admitted: &AdmittedConnection,
    permit: &ChunkSendPermit,
    stop: &TransferStop,
) -> Option<ChunkSendCancelReason> {
    if stop.should_stop() {
        return Some(ChunkSendCancelReason::TransferStopped);
    }
    if let Err(error) = admitted.ensure_current() {
        return match error {
            Error::ConnectionAttemptSuperseded { .. } => {
                Some(ChunkSendCancelReason::AdmissionRevoked(admitted.attempt()))
            }
            error => Some(ChunkSendCancelReason::AdmissionCheckFailed(error)),
        };
    }

    let readiness = admitted.connection().readiness();
    if !readiness.can_make_progress() {
        return Some(ChunkSendCancelReason::TransportNotReady(readiness));
    }

    permit.check().err()
}

fn log_chunk_send_cancel(did: Did, phase: &'static str, reason: &ChunkSendCancelReason) {
    tracing::warn!(
        target: "rings_core::transport::chunked_send",
        peer = %did,
        phase,
        reason = reason.as_str(),
        attempt = ?reason.attempt(),
        transport_readiness = ?reason.transport_readiness(),
        transport_readiness_kind = ?reason
            .transport_readiness()
            .map(TransportReadiness::as_str),
        check_error = ?reason.check_error(),
        records_peer_failure = reason.records_peer_failure(),
        "chunked send cancelled"
    );
}

pub(super) async fn await_delivery_or_cancel(
    delivery: DeliveryFuture,
    admitted: &AdmittedConnection,
    permit: &ChunkSendPermit,
    stop: &TransferStop,
    did: Did,
    phase: &'static str,
) -> ChunkSendProgress<Result<()>> {
    let delivery = delivery.fuse();
    let timeout = sleep(DATA_CHANNEL_DELIVERY_TIMEOUT).fuse();
    pin_mut!(delivery, timeout);

    loop {
        if let Some(reason) = chunk_send_cancel_reason(admitted, permit, stop) {
            return cancel_accepted_delivery(admitted, did, phase, reason).await;
        }

        let poll = sleep(CHUNK_SEND_PERMIT_POLL_INTERVAL).fuse();
        pin_mut!(poll);
        select! {
            result = delivery => {
                if result.is_err() {
                    if let Some(reason) = chunk_send_cancel_reason(admitted, permit, stop) {
                        return cancel_accepted_delivery(admitted, did, phase, reason).await;
                    }
                }
                return ChunkSendProgress::Ready(result.map_err(Error::Transport));
            },
            _ = timeout => {
                if let Err(error) = terminate_accepted_connection(admitted).await {
                    return ChunkSendProgress::Ready(Err(error));
                }
                return ChunkSendProgress::Ready(Err(Error::DataChannelDeliveryTimeout {
                    peer: did,
                    timeout_ms: DATA_CHANNEL_DELIVERY_TIMEOUT.as_millis(),
                    context: phase,
                }));
            },
            _ = poll => {}
        }
    }
}

async fn cancel_accepted_delivery(
    admitted: &AdmittedConnection,
    did: Did,
    phase: &'static str,
    reason: ChunkSendCancelReason,
) -> ChunkSendProgress<Result<()>> {
    log_chunk_send_cancel(did, phase, &reason);
    match terminate_accepted_connection(admitted).await {
        Ok(()) => ChunkSendProgress::Cancelled(reason),
        Err(error) => ChunkSendProgress::Ready(Err(error)),
    }
}

async fn terminate_accepted_connection(admitted: &AdmittedConnection) -> Result<()> {
    admitted.mark_send_terminal()?;
    if !await_bounded_connection_close(admitted.connection().close()).await? {
        tracing::warn!(
            peer = %admitted.attempt().peer(),
            generation = admitted.attempt().generation(),
            timeout_ms = DATA_CHANNEL_CLOSE_TIMEOUT.as_millis(),
            "timed out cleaning up terminal data-channel generation"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_cancel_reports_generation_revocation_explicitly() {
        let attempt = PendingConnectionAttempt {
            peer: Did::from(1_u32),
            generation: 7,
        };

        assert!(matches!(
            ChunkSendCancelReason::AdmissionRevoked(attempt).resolve_initial(),
            Err(Error::ConnectionAttemptSuperseded { peer, generation })
                if peer == attempt.peer() && generation == attempt.generation()
        ));
    }

    #[test]
    fn initial_cancel_treats_route_revocation_as_cancelled() {
        assert!(ChunkSendCancelReason::RouteNoLongerPermitted
            .resolve_initial()
            .is_ok());
    }

    #[test]
    fn initial_cancel_keeps_route_check_error_explicit() {
        let error = Error::InvalidMessage("route check failed".to_string());

        assert!(matches!(
            ChunkSendCancelReason::RouteCheckFailed(error).resolve_initial(),
            Err(Error::InvalidMessage(_))
        ));
    }
}
