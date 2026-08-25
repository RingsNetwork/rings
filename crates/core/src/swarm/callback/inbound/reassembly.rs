use super::decode_payload;
use super::finish_reply;
use super::memory_reservation;
use super::DecodedInboundFrame;
use super::InboundEvent;
use super::InboundFailure;
use super::InboundLane;
use super::InboundProcessor;
use crate::chunk::ReassemblyOutcome;
use crate::chunk::ReassemblyRejection;
use crate::error::Error;
use crate::error::Result;

pub(super) async fn process_chunk_event(
    processor: &InboundProcessor,
    mut event: InboundEvent,
) -> Option<InboundEvent> {
    let chunk = match take_prepared_chunk(&mut event.prepared_message) {
        Ok(chunk) => chunk,
        Err(error) => {
            finish_reply(event.reply, Err(InboundFailure::Core(error)));
            return None;
        }
    };
    let bytes = match processor.handle_chunk(chunk).await {
        ReassemblyOutcome::Complete(bytes) => bytes,
        ReassemblyOutcome::Incomplete
        | ReassemblyOutcome::Rejected(ReassemblyRejection::Capacity)
        | ReassemblyOutcome::Rejected(ReassemblyRejection::Replay) => {
            finish_reply(event.reply, Ok(()));
            return None;
        }
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid) => {
            processor.record_receive_failure(event.peer).await;
            finish_reply(
                event.reply,
                Err(InboundFailure::Core(Error::InvalidChunkMessage)),
            );
            return None;
        }
        ReassemblyOutcome::Rejected(ReassemblyRejection::LocalInvariant) => {
            finish_reply(
                event.reply,
                Err(InboundFailure::Core(Error::InboundActorInvariantViolation)),
            );
            return None;
        }
    };
    let reservation = memory_reservation(bytes.as_ref().len());
    if let Err(error) = event.permit.try_transition(event.lane, reservation) {
        finish_reply(event.reply, Err(InboundFailure::Core(error)));
        return None;
    }
    let DecodedInboundFrame {
        payload,
        prepared_message,
    } = match decode_payload(processor, event.peer, bytes.as_ref()).await {
        Ok(decoded) => decoded,
        Err(error) => {
            finish_reply(event.reply, Err(InboundFailure::Core(error)));
            return None;
        }
    };
    let message = match prepared_message {
        Some(message) => message,
        None => match payload.transaction.data::<crate::message::Message>() {
            Ok(message) => message,
            Err(error) => {
                processor.record_receive_failure(event.peer).await;
                finish_reply(event.reply, Err(InboundFailure::Core(error)));
                return None;
            }
        },
    };
    let meta = crate::message::MessageMeta::from_message(&message);
    if meta.kind().is_chunk() {
        processor.record_receive_failure(event.peer).await;
        finish_reply(
            event.reply,
            Err(InboundFailure::Core(Error::NestedChunkMessage)),
        );
        return None;
    }
    let lane = InboundLane::from_meta(meta);
    if let Err(error) = event.permit.try_transition(lane, reservation) {
        finish_reply(event.reply, Err(InboundFailure::Core(error)));
        return None;
    }
    Some(InboundEvent {
        sequence: event.sequence,
        peer: event.peer,
        payload,
        prepared_message: Some(message),
        lane,
        permit: event.permit,
        reply: event.reply,
    })
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
    fn prepared_chunk_is_moved_without_redecoding_transaction_data() {
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
