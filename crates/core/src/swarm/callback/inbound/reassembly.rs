use super::decode_payload;
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
                payload: reassembled.payload,
                prepared_message: Some(reassembled.message),
                lane: reassembled.lane,
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
    let chunk = take_prepared_chunk(&mut event.prepared_message)?;
    let bytes: crate::chunk::RetainedReassembly = match processor.handle_chunk(chunk).await {
        ReassemblyOutcome::Complete(bytes) => bytes,
        ReassemblyOutcome::Incomplete
        | ReassemblyOutcome::Rejected(ReassemblyRejection::Capacity)
        | ReassemblyOutcome::Rejected(ReassemblyRejection::Replay) => {
            return Ok(None);
        }
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid) => {
            processor.record_receive_failure(event.peer).await;
            return Err(Error::InvalidChunkMessage);
        }
    };
    let reservation = memory_reservation(bytes.as_ref().len());
    event.permit.try_transition(event.lane, reservation)?;
    let payload = decode_payload(processor, event.peer, bytes.as_ref()).await?;
    let message = match payload.transaction.data::<crate::message::Message>() {
        Ok(message) => message,
        Err(error) => {
            processor.record_receive_failure(event.peer).await;
            return Err(error);
        }
    };
    let kind = crate::message::MessageKind::from_message(&message);
    if kind.is_chunk() {
        processor.record_receive_failure(event.peer).await;
        return Err(Error::NestedChunkMessage);
    }
    let lane = InboundLane::from_kind(kind);
    event.permit.try_transition(lane, reservation)?;
    let payload = processor
        .accept_verified_logical_message(event.peer, payload)
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
