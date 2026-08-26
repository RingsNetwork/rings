use std::future::Future;
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
use super::outbound::DetachedAdmission;
use super::outbound::DetachedAdmissionClaim;
use super::AdmittedConnection;
use super::PendingConnectionAttempt;
use super::TransportReadiness;
use super::TRANSPORT_TIMEOUT_PROFILE;
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

pub(super) const DATA_CHANNEL_SEND_ACCEPT_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.send_accept;

#[cfg(test)]
const CHUNK_SEND_PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const CHUNK_SEND_PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

const DATA_CHANNEL_DELIVERY_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.delivery;

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
        match message.storage_sync_destination() {
            Some(destination) => Self::StorageSyncRoute {
                dht,
                destination,
                next_hop,
            },
            None => Self::Always,
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
    detached_admission: Option<&DetachedAdmission>,
    did: Did,
    context: &'static str,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    let bytes = data.len();
    let send_permit = build_transport_send_permit(admitted, permit, stop, detached_admission);
    let acceptance = send_permit.acceptance();
    let send = admitted.connection().send_data(data, send_permit).fuse();
    let timeout = sleep(DATA_CHANNEL_SEND_ACCEPT_TIMEOUT).fuse();
    pin_mut!(send, timeout);

    loop {
        if acceptance.is_irrevocable() {
            return await_irrevocable_send(send, timeout, admitted, did, bytes, context).await;
        }
        if let Some(reason) = chunk_send_cancel_reason(admitted, permit, stop) {
            if acceptance.try_cancel() {
                log_chunk_send_cancel(did, context, &reason);
                return ChunkSendProgress::Cancelled(reason);
            }
            return await_irrevocable_send(send, timeout, admitted, did, bytes, context).await;
        }
        let poll = sleep(CHUNK_SEND_PERMIT_POLL_INTERVAL).fuse();
        pin_mut!(poll);
        select! {
            result = send => {
                if result.is_err() && !acceptance.is_irrevocable() {
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
                if !acceptance.try_cancel() {
                    return expire_irrevocable_send(admitted, did, bytes, context).await;
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

fn build_transport_send_permit(
    admitted: &AdmittedConnection,
    permit: &ChunkSendPermit,
    stop: &TransferStop,
    detached_admission: Option<&DetachedAdmission>,
) -> SendPermit {
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
    let final_admission = admitted.clone();
    let final_route = permit.clone();
    let final_stop = stop.clone();
    let final_detached_admission = detached_admission.cloned();
    send_permit.with_irrevocable_guard(move |claim| {
        let mut claimed = false;
        let mut newly_detached = false;
        let _permitted = final_admission
            .with_current_connection(|connection| {
                final_route.admits(|| {
                    if final_stop.should_stop() || !connection.readiness().can_make_progress() {
                        return false;
                    }
                    if let Some(admission) = &final_detached_admission {
                        let Some(detached_claim) = admission.try_mark_irrevocable() else {
                            return false;
                        };
                        newly_detached = detached_claim == DetachedAdmissionClaim::New;
                    }
                    claimed = claim.try_claim();
                    claimed
                })
            })
            .ok()
            .flatten()
            .unwrap_or(false);
        if !claimed && newly_detached {
            if let Some(admission) = &final_detached_admission {
                admission.rollback_irrevocable_send();
            }
        }
    })
}

async fn await_irrevocable_send(
    send: impl Future<Output = Result<DeliveryFuture>>,
    timeout: impl Future<Output = ()>,
    admitted: &AdmittedConnection,
    did: Did,
    bytes: usize,
    context: &'static str,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    let send = send.fuse();
    let timeout = timeout.fuse();
    pin_mut!(send, timeout);
    select! {
        result = send => complete_irrevocable_send(result, admitted).await,
        _ = timeout => expire_irrevocable_send(admitted, did, bytes, context).await,
    }
}

async fn expire_irrevocable_send(
    admitted: &AdmittedConnection,
    did: Did,
    bytes: usize,
    context: &'static str,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    terminate_accepted_connection(admitted, "send_completion_timeout").await;
    ChunkSendProgress::Ready(Err(Error::DataChannelSendCompletionTimeout {
        peer: did,
        timeout_ms: DATA_CHANNEL_SEND_ACCEPT_TIMEOUT.as_millis(),
        bytes,
        context,
    }))
}

async fn complete_irrevocable_send(
    result: Result<DeliveryFuture>,
    admitted: &AdmittedConnection,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    if result.is_err() {
        terminate_accepted_connection(admitted, "irrevocable_send_error").await;
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
                terminate_accepted_connection(admitted, "delivery_timeout").await;
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
    terminate_accepted_connection(admitted, "accepted_delivery_cancelled").await;
    ChunkSendProgress::Cancelled(reason)
}

pub(super) async fn terminate_accepted_connection(
    admitted: &AdmittedConnection,
    cause: &'static str,
) {
    let TerminalizationOutcome { terminal, close } = attempt_terminalization_and_close(
        || admitted.mark_send_terminal(),
        admitted.connection().close(),
    )
    .await;
    if let Err(error) = terminal {
        log_terminal_cleanup_failure(admitted, cause, "mark_terminal", &error);
    }
    match close {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            peer = %admitted.attempt().peer(),
            generation = admitted.attempt().generation(),
            cause,
            timeout_ms = DATA_CHANNEL_CLOSE_TIMEOUT.as_millis(),
            "timed out cleaning up terminal data-channel generation"
        ),
        Err(error) => log_terminal_cleanup_failure(admitted, cause, "close", &error),
    }
}

struct TerminalizationOutcome {
    terminal: Result<bool>,
    close: Result<bool>,
}

async fn attempt_terminalization_and_close<F>(
    mark_terminal: impl FnOnce() -> Result<bool>,
    close: F,
) -> TerminalizationOutcome
where
    F: Future<Output = Result<()>>,
{
    let terminal = mark_terminal();
    let close = await_bounded_connection_close(close).await;
    TerminalizationOutcome { terminal, close }
}

fn log_terminal_cleanup_failure(
    admitted: &AdmittedConnection,
    cause: &'static str,
    phase: &'static str,
    error: &Error,
) {
    tracing::warn!(
        peer = %admitted.attempt().peer(),
        generation = admitted.attempt().generation(),
        cause,
        phase,
        %error,
        "failed to clean up terminal data-channel generation"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(
        all(feature = "wasm", target_family = "wasm"),
        wasm_bindgen_test::wasm_bindgen_test
    )]
    #[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), tokio::test)]
    async fn terminalization_failure_still_attempts_physical_close() {
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let close_observer = Arc::clone(&closed);

        let TerminalizationOutcome { terminal, close } = attempt_terminalization_and_close(
            || Err(Error::SwarmConnectionLifecycleLock),
            async move {
                close_observer.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            },
        )
        .await;

        assert!(matches!(terminal, Err(Error::SwarmConnectionLifecycleLock)));
        assert!(matches!(close, Ok(true)));
        assert!(closed.load(std::sync::atomic::Ordering::Acquire));
    }

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
