#![allow(clippy::ptr_offset_with_cast)]
#![warn(missing_docs)]
//! An Backend HTTP service handle custom message from `MessageHandler` as CallbackFn.
pub mod http_server;
pub mod text;
pub mod utils;

use std::sync::Arc;

use arrayref::array_refs;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::broadcast::Sender;

use self::http_server::HiddenServerConfig;
use self::http_server::HttpServer;
use self::text::TextEndpoint;
use crate::backend;
use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::backend::types::MessageType;
use crate::prelude::rings_core::message::Message;
use crate::prelude::*;

/// A Backend struct contains http_server.
pub struct Backend {
    http_server: Arc<HttpServer>,
    text_endpoint: TextEndpoint,
    sender: Sender<BackendMessage>,
}

/// BackendConfig
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct BackendConfig {
    /// http_server
    pub hidden_servers: Vec<HiddenServerConfig>,
}

impl From<Vec<HiddenServerConfig>> for BackendConfig {
    fn from(v: Vec<HiddenServerConfig>) -> Self {
        Self { hidden_servers: v }
    }
}

#[cfg(feature = "node")]
impl Backend {
    /// new backend
    /// - `ipfs_gateway`
    pub fn new(config: BackendConfig, sender: Sender<BackendMessage>) -> Self {
        Self {
            http_server: Arc::new(HttpServer::from(config.hidden_servers)),
            text_endpoint: TextEndpoint::default(),
            sender,
        }
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
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        msg: &MaybeEncrypted<CustomMessage>,
    ) {
        let mut relay = ctx.relay.clone();
        relay.relay(relay.destination, None).unwrap();

        let msg = handler.decrypt_msg(msg);
        if let Err(e) = msg {
            tracing::error!("decrypt custom_message failed: {}", e);
            return;
        }
        let msg = msg.unwrap().0;

        let (left, msg) = array_refs![&msg, 4; ..;];
        let (&[flag], _) = array_refs![left, 1, 3];
        if flag != 0 {
            tracing::warn!("invalid custom_message flag: {}", flag);
            return;
        }

        let msg = BackendMessage::try_from(msg);
        if let Err(e) = msg {
            tracing::error!("decode custom_message failed: {}", e);
            return;
        }
        let msg = msg.unwrap();
        tracing::debug!("receive custom_message: {:?}", msg);

        let result = match msg.message_type.into() {
            MessageType::SimpleText => {
                self.text_endpoint
                    .handle_message(handler, ctx, &relay, &msg)
                    .await
            }
            MessageType::HttpRequest => {
                self.http_server
                    .handle_message(handler, ctx, &relay, &msg)
                    .await
            }
            _ => {
                tracing::debug!(
                    "custom_message handle unsupported, tag: {:?}",
                    msg.message_type
                );
                Ok(())
            }
        };
        if let Err(e) = self.sender.send(msg) {
            tracing::error!("broadcast backend_message failed, {}", e);
        }
        if let Err(e) = result {
            tracing::error!("handle custom_message failed: {}", e);
        }
    }

    async fn builtin_message(&self, _handler: &MessageHandler, _ctx: &MessagePayload<Message>) {}
}
