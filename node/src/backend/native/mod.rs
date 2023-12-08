#![warn(missing_docs)]
//! This module provides the implementation of a native Backend,
//! which includes BackendConfig and BackendContext.
//!
//! This module has two submodules: extension and service.
//!
//! The submodule [service] aims to provide an implementation of Rings Network based TCP services.
//! It can forward a TCP request from the Rings Network to a local request.
//!
//! The submodule extension aims to provide an implementation of Rings extensions.
//! These extensions are based on WebAssembly (WASM), allowing downloaded WASM code to be executed
//! as an external extension of the backend.

pub mod extension;
pub mod service;

use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;

use crate::backend::native::extension::Extension;
use crate::backend::native::extension::ExtensionConfig;
use crate::backend::native::service::ServiceConfig;
use crate::backend::native::service::ServiceProvider;
use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::error::Result;
use crate::provider::Provider;

/// BackendConfig including services config and extension config
pub struct BackendConfig {
    /// Config of services
    pub services: Vec<ServiceConfig>,
    /// Config of extensions
    pub extensions: ExtensionConfig,
}

/// BackendContext is a Context holder of backend message handler
pub struct BackendContext {
    server: ServiceProvider,
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
    /// Create a new BackendContext instance with config
    pub async fn new(config: BackendConfig) -> Result<Self> {
        Ok(Self {
            server: ServiceProvider::new(config.services),
            extension: Extension::new(&config.extensions).await?,
        })
    }

    /// List service names
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
