#![allow(clippy::ptr_offset_with_cast)]
#![warn(missing_docs)]
//! An Backend HTTP service handle custom message from `MessageHandler` as CallbackFn.
#[cfg(feature = "node")]
pub mod http_server;
#[cfg(feature = "node")]
pub mod text;

pub mod types;

#[cfg(feature = "node")]
use std::sync::Arc;

#[cfg(feature = "node")]
use arrayref::array_refs;
#[cfg(feature = "node")]
use async_trait::async_trait;
#[cfg(feature = "node")]
use serde::Deserialize;
#[cfg(feature = "node")]
use serde::Serialize;

#[cfg(feature = "node")]
use self::http_server::HiddenServerConfig;
#[cfg(feature = "node")]
use self::http_server::HttpServer;
#[cfg(feature = "node")]
use self::text::TextEndpoint;
#[cfg(feature = "node")]
use self::types::BackendMessage;
#[cfg(feature = "node")]
use self::types::MessageEndpoint;
#[cfg(feature = "node")]
use self::types::MessageType;
#[cfg(feature = "node")]
use crate::prelude::rings_core::message::Message;
#[cfg(feature = "node")]
use crate::prelude::*;

/// A Backend struct contains http_server.
#[cfg(feature = "node")]
pub struct Backend {
    http_server: Arc<HttpServer>,
    text_endpoint: TextEndpoint,
}

/// BackendConfig
#[cfg(feature = "node")]
#[derive(Deserialize, Serialize, Debug)]
pub struct BackendConfig {
    /// http_server
    pub hidden_servers: Vec<HiddenServerConfig>,
}

#[cfg(feature = "node")]
impl Backend {
    /// new backend
    /// - `ipfs_gateway`
    pub fn new(config: BackendConfig) -> Self {
        Self {
            http_server: Arc::new(HttpServer::from(config.hidden_servers)),
            text_endpoint: TextEndpoint::default(),
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
            tracing::warn!("invalid custom_message flag");
            return;
        }

        let msg = BackendMessage::try_from(msg);
        if let Err(e) = msg {
            tracing::error!("decode custom_message failed: {}", e);
            return;
        }
        let msg = msg.unwrap();
        tracing::debug!("receive custom_message: {:?}", msg);

        let result = match msg.message_type {
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
                    "custom_message handle unsupport, tag: {:?}",
                    msg.message_type
                );
                return;
            }
        };
        if let Err(e) = result {
            tracing::error!("handle custom_message failed: {}", e);
        }
    }

    async fn builtin_message(&self, _handler: &MessageHandler, _ctx: &MessagePayload<Message>) {}
}
