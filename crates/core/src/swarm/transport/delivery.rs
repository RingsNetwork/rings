use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::delivery::DeliveryFuture;

use super::SwarmConnection;
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

#[cfg(test)]
pub(super) const DATA_CHANNEL_SEND_ACCEPT_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(not(test))]
pub(super) const DATA_CHANNEL_SEND_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
const CHUNK_SEND_PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const CHUNK_SEND_PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
enum ChunkSendCancelReason {
    TerminalConnectionState(WebrtcConnectionState),
    RouteNoLongerPermitted,
    RouteCheckFailed(Error),
}

impl ChunkSendCancelReason {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::TerminalConnectionState(_) => "terminal_connection_state",
            Self::RouteNoLongerPermitted => "route_no_longer_permitted",
            Self::RouteCheckFailed(_) => "route_check_failed",
        }
    }

    const fn terminal_state(&self) -> Option<WebrtcConnectionState> {
        match self {
            Self::TerminalConnectionState(state) => Some(*state),
            Self::RouteNoLongerPermitted | Self::RouteCheckFailed(_) => None,
        }
    }

    const fn route_check_error(&self) -> Option<&Error> {
        match self {
            Self::RouteCheckFailed(error) => Some(error),
            Self::TerminalConnectionState(_) | Self::RouteNoLongerPermitted => None,
        }
    }

    const fn records_peer_failure(&self) -> bool {
        matches!(self, Self::TerminalConnectionState(_))
    }
}

enum ChunkSendProgress<T> {
    Ready(T),
    Cancelled(ChunkSendCancelReason),
}

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
    pub(super) fn for_payload(dht: Arc<PeerRing>, next_hop: Did, payload: &MessagePayload) -> Self {
        match payload.transaction.data() {
            Ok(Message::SyncEntriesWithSuccessor(msg)) => Self::StorageSyncRoute {
                dht,
                destination: msg.destination,
                next_hop,
            },
            Ok(_) => Self::Always,
            Err(error) => {
                tracing::debug!(
                    target: "rings_core::transport::chunked_send",
                    next_hop = %next_hop,
                    error = ?error,
                    "chunked send route permit fell back to connection-only mode"
                );
                Self::Always
            }
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
}

pub(super) async fn send_data_with_timeout(
    conn: &SwarmConnection,
    data: Bytes,
    did: Did,
    context: &'static str,
) -> Result<DeliveryFuture> {
    let bytes = data.len();
    let send = conn.send_data(data).fuse();
    let timeout = sleep(DATA_CHANNEL_SEND_ACCEPT_TIMEOUT).fuse();
    pin_mut!(send, timeout);

    select! {
        result = send => result,
        _ = timeout => Err(Error::DataChannelSendQueueTimeout {
            peer: did,
            timeout_ms: DATA_CHANNEL_SEND_ACCEPT_TIMEOUT.as_millis(),
            bytes,
            context,
        }),
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
pub(super) fn spawn_delivery(fut: DeliveryFuture, did: Did, measure: Option<MeasureImpl>) {
    wasm_bindgen_futures::spawn_local(async move {
        match fut.await {
            Ok(()) => record_measurement(measure, did, MeasureCounter::Sent).await,
            Err(e) => {
                tracing::warn!("Message to {did} was not delivered: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            }
        }
    });
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, recording
/// the eventual peer-quality observation.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) fn spawn_delivery(fut: DeliveryFuture, did: Did, measure: Option<MeasureImpl>) {
    tokio::spawn(async move {
        match fut.await {
            Ok(()) => record_measurement(measure, did, MeasureCounter::Sent).await,
            Err(e) => {
                tracing::warn!("Message to {did} was not delivered: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            }
        }
    });
}

/// Frame one chunk into the bytes a data-channel send carries: wrap it in a `MessagePayload`
/// addressed to `did` and serialize it. Pure (the only failure is serialization).
pub(super) fn frame_chunk(session_sk: &SessionSk, did: Did, chunk: Chunk) -> Result<Bytes> {
    MessagePayload::new_send(Message::Chunk(chunk), session_sk, did, did)?.to_bincode()
}

/// The *tail* of a chunked message — every chunk after the first — yielded lazily. Boxed so the
/// background task owns a concrete, nameable type (`Send` off the browser, where spawned tasks must
/// be `Send`; single-threaded on it).
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
type ChunkTail = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
type ChunkTail = Box<dyn Iterator<Item = Chunk>>;

fn chunk_send_cancel_reason(
    conn: &SwarmConnection,
    permit: &ChunkSendPermit,
) -> Option<ChunkSendCancelReason> {
    let state = conn.webrtc_connection_state();
    if matches!(
        state,
        WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
    ) {
        return Some(ChunkSendCancelReason::TerminalConnectionState(state));
    }

    permit.check().err()
}

fn log_chunk_send_cancel(did: Did, phase: &'static str, reason: &ChunkSendCancelReason) {
    tracing::warn!(
        target: "rings_core::transport::chunked_send",
        peer = %did,
        phase,
        reason = reason.as_str(),
        terminal_state = ?reason.terminal_state(),
        route_check_error = ?reason.route_check_error(),
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

async fn await_delivery_or_cancel(
    delivery: DeliveryFuture,
    conn: &SwarmConnection,
    permit: &ChunkSendPermit,
    did: Did,
    phase: &'static str,
) -> ChunkSendProgress<Result<()>> {
    let delivery = delivery.fuse();
    pin_mut!(delivery);

    loop {
        if let Some(reason) = chunk_send_cancel_reason(conn, permit) {
            log_chunk_send_cancel(did, phase, &reason);
            return ChunkSendProgress::Cancelled(reason);
        }

        let poll = sleep(CHUNK_SEND_PERMIT_POLL_INTERVAL).fuse();
        pin_mut!(poll);
        select! {
            result = delivery => {
                if result.is_err() {
                    if let Some(reason) = chunk_send_cancel_reason(conn, permit) {
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
    conn: &SwarmConnection,
    bytes: Bytes,
    permit: &ChunkSendPermit,
    did: Did,
) -> ChunkSendProgress<Result<DeliveryFuture>> {
    let send = send_data_with_timeout(conn, bytes, did, "chunked_tail").fuse();
    pin_mut!(send);

    loop {
        if let Some(reason) = chunk_send_cancel_reason(conn, permit) {
            log_chunk_send_cancel(did, "send_chunk", &reason);
            return ChunkSendProgress::Cancelled(reason);
        }

        let poll = sleep(CHUNK_SEND_PERMIT_POLL_INTERVAL).fuse();
        pin_mut!(poll);
        select! {
            result = send => {
                if result.is_err() {
                    if let Some(reason) = chunk_send_cancel_reason(conn, permit) {
                        log_chunk_send_cancel(did, "send_chunk", &reason);
                        return ChunkSendProgress::Cancelled(reason);
                    }
                }
                return ChunkSendProgress::Ready(result);
            },
            _ = poll => {}
        }
    }
}

/// Drive the *tail* of a chunked send: the first chunk has already been accepted by the caller
/// (`do_send_payload`), so wait for it to flush (backpressure), then frame, send, and await each
/// remaining chunk in turn. One chunk is in flight at a time and no per-chunk task is spawned. A
/// later frame/send failure aborts the rest; the receiver TTL-expires the partial message (chunks
/// carry the message ttl), so no abort marker is needed. Fire-and-forget — the caller already
/// learned whether the *first* chunk was accepted, matching the whole-message contract.
async fn run_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
) {
    match await_delivery_or_cancel(first_delivery, &conn, &permit, did, "first_delivery").await {
        ChunkSendProgress::Ready(Ok(())) => {}
        ChunkSendProgress::Ready(Err(e)) => {
            tracing::warn!("Chunked send to {did} stopped before the first chunk flushed: {e}");
            record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            return;
        }
        ChunkSendProgress::Cancelled(reason) => {
            record_cancel_measurement(measure, did, &reason).await;
            return;
        }
    }
    for chunk in tail {
        let bytes = match frame_chunk(&session_sk, did, chunk) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("Chunked send to {did} aborted while framing a chunk: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                return;
            }
        };
        let delivery = match send_chunk_or_cancel(&conn, bytes, &permit, did).await {
            ChunkSendProgress::Ready(Ok(delivery)) => delivery,
            ChunkSendProgress::Ready(Err(e)) => {
                tracing::warn!("Chunked send to {did} stopped: {e}");
                if e.records_peer_send_failure() {
                    record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                }
                return;
            }
            ChunkSendProgress::Cancelled(reason) => {
                record_cancel_measurement(measure, did, &reason).await;
                return;
            }
        };
        match await_delivery_or_cancel(delivery, &conn, &permit, did, "chunk_delivery").await {
            ChunkSendProgress::Ready(Ok(())) => {}
            ChunkSendProgress::Ready(Err(e)) => {
                tracing::warn!("Chunked send to {did} stopped before flush: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                return;
            }
            ChunkSendProgress::Cancelled(reason) => {
                record_cancel_measurement(measure, did, &reason).await;
                return;
            }
        }
    }
    record_measurement(measure, did, MeasureCounter::Sent).await;
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) fn spawn_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
) {
    wasm_bindgen_futures::spawn_local(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
        permit,
        measure,
    ));
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) fn spawn_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
) {
    tokio::spawn(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
        permit,
        measure,
    ));
}
