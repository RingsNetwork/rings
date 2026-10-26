use rings_transport::core::pool::ChannelLane;

use super::finish_completion;
use super::HeldCharge;
use super::InboundEvent;
use super::InboundLane;
use super::InboundProcessor;
use crate::chunk::ReassemblyOutcome;
use crate::chunk::ReassemblyRejection;
use crate::error::Error;
use crate::error::Result;
use crate::measure::Authentication;
use crate::swarm::callback::logical::Arrival;

impl InboundProcessor {
    /// Apply `chunk`, which brought `charge`, to this connection's reassembler: a first chunk's
    /// charge is held with its message, any other chunk's is dropped (#932).
    pub(super) async fn handle_chunk(
        &self,
        (peer, authentication): (Option<crate::dht::Did>, Authentication),
        chunk: crate::chunk::Chunk,
        charge: Option<HeldCharge>,
        carrier: Option<ChannelLane>,
    ) -> ReassemblyOutcome<Option<HeldCharge>> {
        let now_ms = self.reassembly_clock.now_ms();
        let (outcome, expired) = self.reassembler.lock().await.handle_charged_at(
            chunk,
            charge,
            now_ms,
            matches!(authentication, Authentication::Authenticated),
            carrier,
        );
        self.record_expired_reassembly_failures(peer, expired).await;
        outcome
    }

    pub(super) async fn remove_expired_reassembly_at(&self, now_ms: u128) {
        let expired = self.reassembler.lock().await.remove_expired_at(now_ms);
        self.record_expired_reassembly_failures(None, expired).await;
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

/// The reassembler's own state, which the inbound actor tends whether or not it holds the
/// transport bound.
impl<L> InboundProcessor<L> {
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
                carrier: event.carrier,
                wire_bytes: event.wire_bytes,
                permit: event.permit,
                completion: event.completion,
            });
        }
        Err(error) => Err(error),
    };
    finish_completion(event.completion, terminal_reply);
    None
}

async fn advance_chunk_event(
    processor: &InboundProcessor,
    event: &mut InboundEvent,
) -> Result<Option<ReassembledEvent>> {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    let transaction_id = event.payload.transaction.tx_id;
    let chunk = take_prepared_chunk(&mut event.prepared_message)?;
    // The chunk's charge moves into the reassembler: a first chunk's is the whole message's,
    // any other chunk's is covered and holds nothing.
    let charge = event.permit.take();
    let outcome = processor
        .handle_chunk(
            (event.peer, event.authentication),
            chunk,
            charge,
            event.carrier,
        )
        .await;
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if matches!(
        &outcome,
        ReassemblyOutcome::Incomplete | ReassemblyOutcome::Complete(_)
    ) {
        crate::simulation::record_reassembly_advance(transaction_id);
    }
    let retained = match outcome {
        ReassemblyOutcome::Complete(retained) => retained,
        ReassemblyOutcome::Incomplete
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
    // The completed message moves onto the event with the charge its first chunk took for it: a
    // move, not a new charge, so no completed message is refused for capacity (Prop. 4).
    let carrier = retained.carrier();
    let (bytes, charge) = retained.into_parts();
    event.permit.restore(charge);
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
    // Every chunk took credit on the lane they arrived on, which must be the lane of the
    // message they carry (one lane per class).
    if !lane.admits_carrier(carrier) {
        processor
            .record_receive_failure(event.peer, event.authentication)
            .await;
        return Err(Error::InboundLaneMismatch);
    }
    // Chunk envelopes are transport framing. The verified original transaction is admitted here,
    // before it reserves or enters its logical lane, and therefore consumes one quota charge no
    // matter how many envelopes carried it.
    let arrival = Arrival {
        peer: event.peer,
        authentication: event.authentication,
    };
    processor
        .admit_final_transaction(&payload, &message, lane, arrival)
        .await?;
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
