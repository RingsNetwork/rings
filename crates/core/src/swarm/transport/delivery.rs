use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::SendPermit;
use rings_transport::delivery::DeliveryFuture;

use super::AdmittedConnection;
use super::PendingConnectionAttempt;
use super::TransportReadiness;
use crate::chunk::Chunk;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::StorageSyncDestination;
use crate::error::Error;
use crate::error::Result;
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

#[derive(Debug)]
pub(super) enum ChunkSendCancelReason {
    AdmissionRevoked(PendingConnectionAttempt),
    AdmissionCheckFailed(Error),
    TransportNotReady(TransportReadiness),
    RouteNoLongerPermitted,
    RouteCheckFailed(Error),
}

impl ChunkSendCancelReason {
    const fn as_str(&self) -> &'static str {
        match self {
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
            Self::AdmissionRevoked(_)
            | Self::AdmissionCheckFailed(_)
            | Self::RouteNoLongerPermitted
            | Self::RouteCheckFailed(_) => None,
        }
    }

    const fn check_error(&self) -> Option<&Error> {
        match self {
            Self::AdmissionCheckFailed(error) | Self::RouteCheckFailed(error) => Some(error),
            Self::AdmissionRevoked(_)
            | Self::TransportNotReady(_)
            | Self::RouteNoLongerPermitted => None,
        }
    }

    pub(super) const fn records_peer_failure(&self) -> bool {
        match self {
            Self::TransportNotReady(readiness) => readiness.is_terminal(),
            Self::AdmissionRevoked(_)
            | Self::AdmissionCheckFailed(_)
            | Self::RouteNoLongerPermitted
            | Self::RouteCheckFailed(_) => false,
        }
    }

    const fn attempt(&self) -> Option<PendingConnectionAttempt> {
        match self {
            Self::AdmissionRevoked(attempt) => Some(*attempt),
            Self::AdmissionCheckFailed(_)
            | Self::TransportNotReady(_)
            | Self::RouteNoLongerPermitted
            | Self::RouteCheckFailed(_) => None,
        }
    }

    /// Resolve cancellation before the first frame has been accepted.
    ///
    /// A vanished storage route is a normal maintenance deferral. Every other
    /// reason is an explicit failure of the connection or route check that
    /// admitted the send.
    pub(super) fn resolve_initial(self) -> Result<()> {
        match self {
            Self::AdmissionRevoked(attempt) => Err(Error::ConnectionAttemptSuperseded {
                peer: attempt.peer(),
                generation: attempt.generation(),
            }),
            Self::AdmissionCheckFailed(error) | Self::RouteCheckFailed(error) => Err(error),
            Self::TransportNotReady(readiness) => Err(Error::TransportNotReady {
                state: readiness.state(),
                data_channel_open: readiness.data_channel_open(),
            }),
            Self::RouteNoLongerPermitted => Ok(()),
        }
    }
}

pub(super) enum ChunkSendProgress<T> {
    Ready(T),
    Cancelled(ChunkSendCancelReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendCompletionOutcome {
    /// The boundary selected by the completion policy was reached: transport
    /// acceptance for detached sends, full delivery for tracked sends.
    Succeeded,
    /// A generation, readiness, route, or tracked-delivery condition was revoked.
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

    pub(super) const fn records_missing_connection_failure(&self) -> bool {
        matches!(self, Self::Always)
    }
}

pub(super) async fn send_data_with_timeout(
    admitted: &AdmittedConnection,
    data: Bytes,
    permit: &ChunkSendPermit,
    did: Did,
    context: &'static str,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    let bytes = data.len();
    let admission = admitted.clone();
    let route = permit.clone();
    let send_permit = SendPermit::new(move || {
        admission
            .with_current(|connection| route.admits(|| connection.readiness().can_make_progress()))
            .ok()
            .flatten()
            .unwrap_or(false)
    });
    let send = admitted.connection().send_data(data, send_permit).fuse();
    let timeout = sleep(DATA_CHANNEL_SEND_ACCEPT_TIMEOUT).fuse();
    pin_mut!(send, timeout);

    loop {
        if let Some(reason) = chunk_send_cancel_reason(admitted, permit) {
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
                    if let Some(reason) = chunk_send_cancel_reason(admitted, permit) {
                        log_chunk_send_cancel(did, context, &reason);
                        return ChunkSendProgress::Cancelled(reason);
                    }
                }
                return ChunkSendProgress::Ready(result);
            },
            _ = timeout => {
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

pub(super) async fn record_measurement(
    measure: Option<MeasureImpl>,
    did: Did,
    counter: MeasureCounter,
) {
    if let Some(measure) = measure {
        measure.incr(did, counter).await;
    }
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, recording
/// the eventual peer-quality observation. This keeps delivery tracking confined
/// to the send site: the status never propagates up through the swarm/node
/// layers.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) fn spawn_delivery(
    fut: DeliveryFuture,
    admitted: AdmittedConnection,
    permit: ChunkSendPermit,
    did: Did,
    measure: Option<MeasureImpl>,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = await_tracked_delivery(fut, &admitted, &permit, did, measure).await;
    });
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, recording
/// the eventual peer-quality observation.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) fn spawn_delivery(
    fut: DeliveryFuture,
    admitted: AdmittedConnection,
    permit: ChunkSendPermit,
    did: Did,
    measure: Option<MeasureImpl>,
) {
    tokio::spawn(async move {
        let _ = await_tracked_delivery(fut, &admitted, &permit, did, measure).await;
    });
}

/// Frame one chunk into the bytes a data-channel send carries: wrap it in a `MessagePayload`
/// addressed to `did` and serialize it. Pure (the only failure is serialization).
pub(super) fn frame_chunk(session_sk: &SessionSk, did: Did, chunk: Chunk) -> Result<Bytes> {
    MessagePayload::new_send(Message::Chunk(chunk), session_sk, did, did)?.to_wire()
}

/// The *tail* of a chunked message — every chunk after the first — yielded lazily. Boxed so the
/// background task owns a concrete, nameable type (`Send` off the browser, where spawned tasks must
/// be `Send`; single-threaded on it).
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
type ChunkTail = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
type ChunkTail = Box<dyn Iterator<Item = Chunk>>;

fn chunk_send_cancel_reason(
    admitted: &AdmittedConnection,
    permit: &ChunkSendPermit,
) -> Option<ChunkSendCancelReason> {
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

async fn record_cancel_measurement(
    measure: Option<MeasureImpl>,
    did: Did,
    reason: &ChunkSendCancelReason,
) {
    if reason.records_peer_failure() {
        record_measurement(measure, did, MeasureCounter::FailedToSend).await;
    }
}

/// Await one complete tracked delivery or its route cancellation.
pub(super) async fn await_tracked_delivery(
    delivery: DeliveryFuture,
    admitted: &AdmittedConnection,
    permit: &ChunkSendPermit,
    did: Did,
    measure: Option<MeasureImpl>,
) -> Result<SendCompletionOutcome> {
    match await_delivery_or_cancel(delivery, admitted, permit, did, "tracked_delivery").await {
        ChunkSendProgress::Ready(Ok(())) => {
            record_measurement(measure, did, MeasureCounter::Sent).await;
            Ok(SendCompletionOutcome::Succeeded)
        }
        ChunkSendProgress::Ready(Err(error)) => {
            tracing::warn!("Tracked message to {did} was not delivered: {error}");
            record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            Err(error)
        }
        ChunkSendProgress::Cancelled(reason) => {
            record_cancel_measurement(measure, did, &reason).await;
            Ok(SendCompletionOutcome::Cancelled)
        }
    }
}

async fn await_delivery_or_cancel(
    delivery: DeliveryFuture,
    admitted: &AdmittedConnection,
    permit: &ChunkSendPermit,
    did: Did,
    phase: &'static str,
) -> ChunkSendProgress<Result<()>> {
    let delivery = delivery.fuse();
    pin_mut!(delivery);

    loop {
        if let Some(reason) = chunk_send_cancel_reason(admitted, permit) {
            log_chunk_send_cancel(did, phase, &reason);
            return ChunkSendProgress::Cancelled(reason);
        }

        let poll = sleep(CHUNK_SEND_PERMIT_POLL_INTERVAL).fuse();
        pin_mut!(poll);
        select! {
            result = delivery => {
                if result.is_err() {
                    if let Some(reason) = chunk_send_cancel_reason(admitted, permit) {
                        log_chunk_send_cancel(did, phase, &reason);
                        return ChunkSendProgress::Cancelled(reason);
                    }
                }
                return ChunkSendProgress::Ready(result.map_err(Error::Transport));
            },
            _ = poll => {}
        }
    }
}

async fn send_chunk_or_cancel(
    admitted: &AdmittedConnection,
    bytes: Bytes,
    permit: &ChunkSendPermit,
    did: Did,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    send_data_with_timeout(admitted, bytes, permit, did, "chunked_tail").await
}

/// Drive the *tail* of a chunked send: the first chunk has already been accepted by the caller
/// (`do_send_payload`), so wait for it to flush (backpressure), then frame, send, and await each
/// remaining chunk in turn. One chunk is in flight at a time and no per-chunk task is spawned. A
/// later frame/send failure aborts the rest; the receiver TTL-expires the partial message (chunks
/// carry the message ttl), so no abort marker is needed. Callers choose whether
/// to await this future or detach it after first-chunk admission.
pub(super) async fn run_chunked_send(
    admitted: AdmittedConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
) -> Result<SendCompletionOutcome> {
    match await_delivery_or_cancel(first_delivery, &admitted, &permit, did, "first_delivery").await
    {
        ChunkSendProgress::Ready(Ok(())) => {}
        ChunkSendProgress::Ready(Err(e)) => {
            tracing::warn!("Chunked send to {did} stopped before the first chunk flushed: {e}");
            record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            return Err(e);
        }
        ChunkSendProgress::Cancelled(reason) => {
            record_cancel_measurement(measure, did, &reason).await;
            return Ok(SendCompletionOutcome::Cancelled);
        }
    }
    for chunk in tail {
        let bytes = match frame_chunk(&session_sk, did, chunk) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("Chunked send to {did} aborted while framing a chunk: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                return Err(e);
            }
        };
        let delivery = match send_chunk_or_cancel(&admitted, bytes, &permit, did).await {
            ChunkSendProgress::Ready(Ok(delivery)) => delivery,
            ChunkSendProgress::Ready(Err(e)) => {
                tracing::warn!("Chunked send to {did} stopped: {e}");
                if e.records_peer_send_failure() {
                    record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                }
                return Err(e);
            }
            ChunkSendProgress::Cancelled(reason) => {
                record_cancel_measurement(measure, did, &reason).await;
                return Ok(SendCompletionOutcome::Cancelled);
            }
        };
        match await_delivery_or_cancel(delivery, &admitted, &permit, did, "chunk_delivery").await {
            ChunkSendProgress::Ready(Ok(())) => {}
            ChunkSendProgress::Ready(Err(e)) => {
                tracing::warn!("Chunked send to {did} stopped before flush: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                return Err(e);
            }
            ChunkSendProgress::Cancelled(reason) => {
                record_cancel_measurement(measure, did, &reason).await;
                return Ok(SendCompletionOutcome::Cancelled);
            }
        }
    }
    record_measurement(measure, did, MeasureCounter::Sent).await;
    Ok(SendCompletionOutcome::Succeeded)
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) fn spawn_chunked_send(
    admitted: AdmittedConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = run_chunked_send(
            admitted,
            tail,
            first_delivery,
            session_sk,
            did,
            permit,
            measure,
        )
        .await;
    });
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) fn spawn_chunked_send(
    admitted: AdmittedConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
) {
    tokio::spawn(async move {
        let _ = run_chunked_send(
            admitted,
            tail,
            first_delivery,
            session_sk,
            did,
            permit,
            measure,
        )
        .await;
    });
}
