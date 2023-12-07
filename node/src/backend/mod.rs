pub mod types;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::callback::SwarmCallback;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::client::Client;
use crate::error::Result;

#[cfg(feature = "browser")]
pub mod browser;

#[cfg(feature = "node")]
pub mod native;

#[cfg(feature = "node")]
type HandlerTrait = dyn MessageEndpoint<BackendMessage> + Send + Sync;
#[cfg(feature = "browser")]
type HandlerTrait = dyn MessageEndpoint<BackendMessage>;

pub struct Backend {
    client: Arc<Client>,
    handler: Box<HandlerTrait>,
}

impl Backend {
    pub fn new(client: Arc<Client>, handler: Box<HandlerTrait>) -> Self {
        Self { client, handler }
    }

    async fn handle_backend_message(
        &self,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
        let client = self.client.clone();
        self.handler.handle_message(client, payload, msg).await
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl SwarmCallback for Backend {
    async fn on_inbound(
        &self,

        payload: &MessagePayload,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let data: Message = payload.transaction.data()?;

        let Message::CustomMessage(CustomMessage(msg)) = data else {
            return Ok(());
        };

        let backend_msg = bincode::deserialize(&msg)?;
        tracing::debug!("backend_message received: {backend_msg:?}");

        self.handle_backend_message(payload, &backend_msg).await?;

        Ok(())
    }
}
