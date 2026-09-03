use super::finish_reply;
use super::memory_reservation;
use super::InboundEvent;
use super::InboundFailure;
use super::InboundLane;
use super::InboundProcessor;
use crate::chunk::ReassemblyOutcome;
use crate::chunk::ReassemblyRejection;
use crate::error::Error;
use crate::error::Result;
use crate::measure::Authentication;

impl InboundProcessor {
    pub(super) async fn handle_chunk(
        &self,
        peer: Option<crate::dht::Did>,
        authentication: Authentication,
        chunk: crate::chunk::Chunk,
    ) -> ReassemblyOutcome {
        let now_ms = self.reassembly_clock.now_ms();
        let (outcome, expired) = self
            .reassembler
            .lock()
            .await
            .handle_retained_at_with_attribution(
                chunk,
                now_ms,
                matches!(authentication, Authentication::Authenticated),
            );
        self.record_expired_reassembly_failures(peer, expired).await;
        outcome
    }

    pub(super) async fn remove_expired_reassembly_at(&self, now_ms: u128) {
        let expired = self.reassembler.lock().await.remove_expired_at(now_ms);
        self.record_expired_reassembly_failures(None, expired).await;
    }

    pub(super) async fn has_pending_reassembly(&self) -> bool {
        self.reassembler.lock().await.has_pending()
    }

    pub(super) async fn prepare_reassembly_for_close(&self) -> bool {
        self.reassembler.lock().await.prepare_for_close()
    }

    pub(super) async fn discard_reassembly_after_close_timer_failure(&self) {
        self.reassembler
            .lock()
            .await
            .discard_after_close_timer_failure();
    }

    async fn record_expired_reassembly_failures(
        &self,
        fallback_peer: Option<crate::dht::Did>,
        count: usize,
    ) {
        if count == 0 {
            return;
        }
        let peer = self
            .pending_attempt()
            .map(|attempt| attempt.peer())
            .or(fallback_peer);
        let Some(peer) = peer else {
            tracing::warn!(
                expired_messages = count,
                "expired incomplete reassemblies had no authenticated peer"
            );
            return;
        };
        // Pre: `count` includes only pending entries marked peer-attributable at
        // authenticated ingress, and one reassembler serves one connection peer.
        for _ in 0..count {
            self.record_receive_failure(Some(peer), Authentication::Authenticated)
                .await;
        }
    }
}

struct ReassembledEvent {
    payload: crate::message::MessagePayload,
    message: crate::message::Message,
    lane: InboundLane,
}

pub(super) async fn process_chunk_event(
    processor: &InboundProcessor,
    mut event: InboundEvent,
) -> Option<InboundEvent> {
    let terminal_reply = match advance_chunk_event(processor, &mut event).await {
        Ok(None) => Ok(()),
        Ok(Some(reassembled)) => {
            return Some(InboundEvent {
                sequence: event.sequence,
                peer: event.peer,
                authentication: event.authentication,
                payload: reassembled.payload,
                prepared_message: Some(reassembled.message),
                lane: reassembled.lane,
                wire_bytes: event.wire_bytes,
                permit: event.permit,
                reply: event.reply,
            });
        }
        Err(error) => Err(InboundFailure::Core(error)),
    };
    finish_reply(event.reply, terminal_reply);
    None
}

async fn advance_chunk_event(
    processor: &InboundProcessor,
    event: &mut InboundEvent,
) -> Result<Option<ReassembledEvent>> {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    let transaction_id = event.payload.transaction.tx_id;
    let chunk = take_prepared_chunk(&mut event.prepared_message)?;
    let outcome = processor
        .handle_chunk(event.peer, event.authentication, chunk)
        .await;
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if matches!(
        &outcome,
        ReassemblyOutcome::Incomplete | ReassemblyOutcome::Complete(_)
    ) {
        crate::simulation::record_reassembly_advance(transaction_id);
    }
    let bytes: crate::chunk::RetainedReassembly = match outcome {
        ReassemblyOutcome::Complete(bytes) => bytes,
        ReassemblyOutcome::Incomplete
        | ReassemblyOutcome::Rejected(ReassemblyRejection::Capacity)
        | ReassemblyOutcome::Rejected(ReassemblyRejection::Replay) => {
            return Ok(None);
        }
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid) => {
            processor
                .record_receive_failure(event.peer, event.authentication)
                .await;
            return Err(Error::InvalidChunkMessage);
        }
    };
    let reservation = memory_reservation(bytes.as_ref().len());
    event.permit.try_transition(event.lane, reservation)?;
    let payload = processor
        .decode_verified_payload(event.peer, event.authentication, bytes.as_ref())
        .await?;
    let message = match payload.transaction.data::<crate::message::Message>() {
        Ok(message) => message,
        Err(error) => {
            processor
                .record_receive_failure(event.peer, event.authentication)
                .await;
            return Err(error);
        }
    };
    let kind = crate::message::MessageKind::from_message(&message);
    if kind.is_chunk() {
        processor
            .record_receive_failure(event.peer, event.authentication)
            .await;
        return Err(Error::NestedChunkMessage);
    }
    let lane = InboundLane::from_kind(kind);
    event.permit.try_transition(lane, reservation)?;
    let payload = processor
        .accept_verified_logical_message(event.peer, event.authentication, payload)
        .await?;
    Ok(Some(ReassembledEvent {
        payload,
        message,
        lane,
    }))
}

fn take_prepared_chunk(
    prepared_message: &mut Option<crate::message::Message>,
) -> Result<crate::chunk::Chunk> {
    match prepared_message.take() {
        Some(crate::message::Message::Chunk(chunk)) => Ok(chunk),
        _ => Err(Error::InboundActorInvariantViolation),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::chunk::Chunk;
    use crate::chunk::ChunkMeta;
    use crate::message::Message;

    #[test]
    fn test_prepared_chunk_is_moved_without_redecoding_transaction_data() {
        let meta = ChunkMeta::default();
        let mut prepared_message = Some(Message::Chunk(Chunk {
            chunk: [1, 3],
            data: Bytes::from_static(b"prepared"),
            meta,
        }));

        let chunk = take_prepared_chunk(&mut prepared_message)
            .expect("a prepared chunk must be consumed directly");

        assert_eq!(chunk.chunk, [1, 3]);
        assert_eq!(chunk.data, Bytes::from_static(b"prepared"));
        assert_eq!(chunk.meta.id, meta.id);
        assert!(prepared_message.is_none());
    }
}
