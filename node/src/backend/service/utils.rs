#![warn(missing_docs)]
//! utils of service
use crate::prelude::*;

/// send chunk report message
pub fn send_chunk_report_message(
    ctx: &MessagePayload<Message>,
    data: &[u8],
) -> MessageHandlerEvent {
    let mut new_bytes: Vec<u8> = Vec::with_capacity(data.len() + 1);
    new_bytes.push(1);
    new_bytes.extend_from_slice(&[0u8; 3]);
    new_bytes.extend_from_slice(data);

    MessageHandlerEvent::SendReportMessage(ctx.clone(), Message::custom(&new_bytes))
}
