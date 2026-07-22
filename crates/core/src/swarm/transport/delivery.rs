use bytes::Bytes;
use rings_transport::delivery::DeliveryFuture;

use super::SwarmConnection;
use crate::chunk::Chunk;
use crate::dht::Did;
use crate::error::Result;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::session::SessionSk;

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
    measure: Option<MeasureImpl>,
) {
    if let Err(e) = first_delivery.await {
        tracing::warn!("Chunked send to {did} stopped before the first chunk flushed: {e}");
        record_measurement(measure, did, MeasureCounter::FailedToSend).await;
        return;
    }
    for chunk in tail {
        let bytes = match frame_chunk(&session_sk, did, chunk) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("Chunked send to {did} aborted while framing a chunk: {e}");
                return;
            }
        };
        match conn.send_data(bytes).await {
            Ok(delivery) => {
                if let Err(e) = delivery.await {
                    tracing::warn!("Chunked send to {did} stopped before flush: {e}");
                    record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                    return;
                }
            }
            Err(e) => {
                tracing::warn!("Chunked send to {did} stopped: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
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
    measure: Option<MeasureImpl>,
) {
    wasm_bindgen_futures::spawn_local(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
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
    measure: Option<MeasureImpl>,
) {
    tokio::spawn(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
        measure,
    ));
}
