pub mod extension;
pub mod server;

use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;

use crate::backend::native::extension::Extension;
use crate::backend::native::extension::ExtensionConfig;
use crate::backend::native::server::Server;
use crate::backend::native::server::ServiceConfig;
use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::provider::Provider;
use crate::error::Result;

pub struct BackendConfig {
    pub services: Vec<ServiceConfig>,
    pub extensions: ExtensionConfig,
}

pub struct BackendContext {
    server: Server,
    extension: Extension,
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MessageEndpoint<BackendMessage> for BackendContext {
    async fn on_message(
        &self,
        provider: Arc<Provider>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
        self.handle_backend_message(provider, payload, msg).await
    }
}

impl BackendContext {
    pub async fn new(config: BackendConfig) -> Result<Self> {
        Ok(Self {
            server: Server::new(config.services),
            extension: Extension::new(&config.extensions).await?,
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
        provider: Arc<Provider>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
        match msg {
            BackendMessage::Extension(data) => {
                self.extension.on_message(provider, payload, data).await
            }
            BackendMessage::ServerMessage(data) => {
                self.server.on_message(provider, payload, data).await
            }
            BackendMessage::PlainText(text) => {
                let peer_did = payload.transaction.signer();
                tracing::info!("BackendMessage from {peer_did:?} PlainText: {text:?}");
                Ok(())
            }
        }
    }
}
