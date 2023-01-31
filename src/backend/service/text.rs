#![warn(missing_docs)]
//! handle simple text message
use std::str;

use async_trait::async_trait;

use super::backend_message::BackendMessage;
use super::backend_message::MessageEndpoint;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;

/// SimpleTextEndpoint
#[derive(Clone, Debug, Default)]
pub struct TextEndpoint;

#[async_trait]
impl MessageEndpoint for TextEndpoint {
    async fn handle_message(
        &self,
        _handler: &MessageHandler,
        _ctx: &MessagePayload<Message>,
        relay: &MessageRelay,
        data: &BackendMessage,
    ) -> Result<()> {
        let text = str::from_utf8(data.data.as_slice()).map_err(|_| Error::InvalidMessage)?;
        tracing::info!("SimpleText, From: {}, Text: {}", relay.sender(), text);
        Ok(())
    }
}
