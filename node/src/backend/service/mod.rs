#![allow(clippy::ptr_offset_with_cast)]
#![warn(missing_docs)]
//! An Backend HTTP service handle custom message from `MessageHandler` as CallbackFn.
pub mod http_server;
pub mod text;
pub mod utils;

use std::sync::Arc;

use arrayref::array_refs;
use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::broadcast::Sender;
use tokio::sync::Mutex;

use self::http_server::HiddenServerConfig;
use self::http_server::HttpServer;
use self::text::TextEndpoint;
use crate::backend;
use crate::backend::extension::Extension;
use crate::backend::extension::ExtensionConfig;
use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::backend::types::MessageType;
use crate::consts::BACKEND_MTU;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::chunk::Chunk;
use crate::prelude::rings_core::chunk::ChunkList;
use crate::prelude::rings_core::chunk::ChunkManager;
use crate::prelude::rings_core::message::Message;
use crate::prelude::*;

/// A Backend struct contains http_server.
pub struct Backend {
    http_server: Arc<HttpServer>,
    text_endpoint: TextEndpoint,
    extension_endpoint: Extension,
    sender: Sender<BackendMessage>,
    chunk_list: Arc<Mutex<ChunkList<BACKEND_MTU>>>,
}

/// BackendConfig
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct BackendConfig {
    /// http_server
    pub hidden_servers: Vec<HiddenServerConfig>,
    /// extension
    pub extensions: ExtensionConfig,
}

impl From<(Vec<HiddenServerConfig>, ExtensionConfig)> for BackendConfig {
    fn from((s, e): (Vec<HiddenServerConfig>, ExtensionConfig)) -> Self {
        Self {
            hidden_servers: s,
            extensions: e,
        }
    }
}

#[cfg(feature = "node")]
impl Backend {
    /// new backend
    /// - `ipfs_gateway`
    pub async fn new(config: BackendConfig, sender: Sender<BackendMessage>) -> Result<Self> {
        Ok(Self {
            http_server: Arc::new(HttpServer::from(config.hidden_servers)),
            text_endpoint: TextEndpoint,
            sender,
            extension_endpoint: Extension::new(&config.extensions).await?,
            chunk_list: Default::default(),
        })
    }

    async fn handle_chunk_data(&self, data: &[u8]) -> Result<Option<Bytes>> {
        let chunk_item = Chunk::from_bincode(data).map_err(|_| Error::DecodeError)?;
        let mut chunk_list = self.chunk_list.lock().await;
        let data = chunk_list.handle(chunk_item);
        Ok(data)
    }

    /// Get service names from http_server config for storage register.
    pub fn service_names(&self) -> Vec<String> {
        self.http_server
            .services
            .iter()
            .cloned()
            .filter_map(|b| b.register_service)
            .collect::<Vec<_>>()
    }
}

#[cfg(feature = "node")]
#[async_trait]
impl MessageCallback for Backend {
    /// `custom_message` in Backend for now only handle
    /// the `Message::CustomMessage` in handler `invoke_callback` function.
    /// And send http request to localhost through Backend http_request handler.
    async fn custom_message(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &CustomMessage,
    ) -> Vec<MessageHandlerEvent> {
        let msg = msg.0.clone();

        let (left, msg) = array_refs![&msg, 4; ..;];
        let (&[flag], _) = array_refs![left, 1, 3];

        let msg = if flag == 1 {
            let data = self.handle_chunk_data(msg).await;
            if let Err(e) = data {
                tracing::error!("handle_chunk_data failed: {}", e);
                return vec![];
            }
            let data = data.unwrap();
            if let Some(data) = data {
                BackendMessage::try_from(data.to_vec().as_ref())
            } else {
                return vec![];
            }
        } else if flag == 0 {
            BackendMessage::try_from(msg)
        } else {
            tracing::warn!("invalid custom_message flag: {}", flag);
            return vec![];
        };

        if let Err(e) = msg {
            tracing::error!("decode custom_message failed: {}", e);
            return vec![];
        }
        let msg = msg.unwrap();
        tracing::debug!("receive custom_message: {:?}", msg);

        let result = match msg.message_type.into() {
            MessageType::SimpleText => self.text_endpoint.handle_message(ctx, &msg).await,
            MessageType::HttpRequest => self.http_server.handle_message(ctx, &msg).await,
            MessageType::Extension => self.extension_endpoint.handle_message(ctx, &msg).await,
            _ => {
                tracing::debug!(
                    "custom_message handle unsupported, tag: {:?}",
                    msg.message_type
                );
                Ok(vec![])
            }
        };
        if let Err(e) = self.sender.send(msg) {
            tracing::error!("broadcast backend_message failed, {}", e);
        }

        match result {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("handle custom_message failed: {}", e);
                vec![]
            }
        }
    }

    async fn builtin_message(&self, _ctx: &MessagePayload<Message>) -> Vec<MessageHandlerEvent> {
        vec![]
    }
}
