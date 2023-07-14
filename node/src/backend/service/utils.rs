#![warn(missing_docs)]
//! utils of service
use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;

/// send chunk report message
pub async fn send_chunk_report_message(
    ctx: &MessagePayload<Message>,
    data: &[u8],
) -> Result<MessageHandlerEvent> {
    let mut new_bytes: Vec<u8> = Vec::with_capacity(data.len() + 1);
    new_bytes.push(1);
    new_bytes.extend_from_slice(&[0u8; 3]);
    new_bytes.extend_from_slice(data);

    Ok(MessageHandlerEvent::SendReportMessage(
        ctx.clone(),
        Message::custom(&new_bytes).map_err(|_| Error::InvalidMessage)?,
    ))
}
