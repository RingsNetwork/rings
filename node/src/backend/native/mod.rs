pub mod extension;
pub mod server;

use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::callback::SwarmCallback;

use crate::error::Result;
use crate::backend::native::extension::Extension;
use crate::backend::native::extension::ExtensionConfig;
use crate::backend::native::server::Server;
use crate::backend::native::server::ServiceConfig;
use crate::processor::Processor;
use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;

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

    async fn handle_backend_message(
        &self,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
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
        tracing::debug!("backend_message received: {backend_msg:?}");

        self.handle_backend_message(payload, &backend_msg).await?;

        Ok(())
    }
}
