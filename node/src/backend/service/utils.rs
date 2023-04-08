#![warn(missing_docs)]
//! utils of service
use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;

/// send chunk report message
/// - `handler`
/// - `ctx`
/// - `data`
pub async fn send_chunk_report_message(
    handler: &MessageHandler,
    ctx: &MessagePayload<Message>,
    data: &[u8],
) -> Result<()> {
    let mut new_bytes: Vec<u8> = Vec::with_capacity(data.len() + 1);
    new_bytes.push(1);
    new_bytes.extend_from_slice(&[0u8; 3]);
    new_bytes.extend_from_slice(data);

    handler
        .send_report_message(
            ctx,
            Message::custom(&new_bytes, None).map_err(|_| Error::InvalidMessage)?,
        )
        .await
        .map_err(Error::SendMessage)?;
    Ok(())
}
