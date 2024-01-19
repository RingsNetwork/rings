#![warn(missing_docs)]
//! This module provide basic mechanism.

pub mod snark;
pub mod types;
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::callback::SwarmCallback;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageHandler;
use crate::provider::Provider;

#[cfg(feature = "browser")]
pub mod browser;

#[cfg(feature = "node")]
pub mod native;

#[cfg(feature = "ffi")]
pub mod ffi;

#[cfg(feature = "node")]
type HandlerTrait = dyn MessageHandler<BackendMessage> + Send + Sync;
#[cfg(feature = "browser")]
type HandlerTrait = dyn MessageHandler<BackendMessage>;

/// Backend handle custom messages from Swarm
pub struct Backend {
    provider: Arc<Provider>,
    handler: Box<HandlerTrait>,
}

impl Backend {
    /// Create a new backend instance with Provider and Handler functions
    pub fn new(provider: Arc<Provider>, handler: Box<HandlerTrait>) -> Self {
        Self { provider, handler }
    }

    async fn on_backend_message(
        &self,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let provider = self.provider.clone();
        self.handler.handle_message(provider, payload, msg).await
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl SwarmCallback for Backend {
    async fn on_inbound(&self, payload: &MessagePayload) -> Result<(), Box<dyn std::error::Error>> {
        let data: Message = payload.transaction.data()?;

        let Message::CustomMessage(CustomMessage(msg)) = data else {
            return Ok(());
        };

        let backend_msg = bincode::deserialize(&msg)?;
        tracing::debug!("backend_message received: {backend_msg:?}");

        self.on_backend_message(payload, &backend_msg).await?;

        Ok(())
    }
}
