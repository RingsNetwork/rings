pub mod extension;
#[cfg(feature = "node")]
pub mod server;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::callback::SwarmCallback;

use crate::backend::extension::Extension;
use crate::backend::extension::ExtensionConfig;
use crate::backend::server::Server;
use crate::backend::server::ServiceConfig;
use crate::backend::types::BackendMessage;
use crate::backend::types::IntoBackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

pub struct BackendConfig {
    pub services: Vec<ServiceConfig>,
    pub extensions: ExtensionConfig,
}

pub struct Backend {
    server: Server,
    extension: Extension,
}

impl Backend {
    pub async fn new(config: BackendConfig, processor: Arc<Processor>) -> Result<Self> {
        Ok(Self {
            server: Server::new(config.services, processor.clone()),
            extension: Extension::new(&config.extensions, processor.clone()).await?,
        })
    }

    pub fn service_names(&self) -> Vec<String> {
        self.server
            .services
            .iter()
            .filter_map(|x| x.register_service.clone())
            .collect()
    }

    async fn handle_message(&self, payload: &MessagePayload, msg: &BackendMessage) -> Result<()> {
        match msg {
            BackendMessage::Extension(data) => self.extension.handle_message(payload, data).await,
            BackendMessage::ServerMessage(data) => self.server.handle_message(payload, data).await,
        }
    }
}

#[async_trait]
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
        tracing::debug!("received custom_message: {:?}", msg);

        self.handle_message(payload, &backend_msg).await?;

        Ok(())
    }
}

impl Processor {
    async fn send_backend_message(
        &self,
        destination: Did,
        msg: impl IntoBackendMessage,
    ) -> Result<uuid::Uuid> {
        let backend_msg = msg.into_backend_message();
        let msg_bytes = bincode::serialize(&backend_msg).map_err(|_| Error::EncodeError)?;
        self.send_message(&destination.to_string(), &msg_bytes)
            .await
    }
}
