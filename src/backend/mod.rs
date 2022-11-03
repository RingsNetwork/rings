//! An Backend HTTP service handle custom message from `MessageHandler` as CallbackFn.
/// The trait `rings_core::handlers::MessageCallback` is implemented on `Backend` type
/// To indicate to handle custom message relay, which use as a callbackFn in `MessageHandler`
pub mod http_server;
// pub mod http_server;
#[cfg(feature = "node")]
pub mod ipfs;
#[cfg(feature = "node")]
pub mod text;

pub mod types;

#[cfg(feature = "node")]
use async_trait::async_trait;

#[cfg(feature = "node")]
use self::ipfs::IpfsEndpoint;
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
    ipfs_endpoint: Option<IpfsEndpoint>,
    text_endpoint: TextEndpoint,
}

#[cfg(feature = "node")]
impl Backend {
    pub fn new(ipfs_gateway: Option<String>) -> Self {
        Self {
            ipfs_endpoint: ipfs_gateway.map(|s| IpfsEndpoint::new(s.as_str())),
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

        let msg = BackendMessage::try_from(msg.as_slice());
        if let Err(e) = msg {
            tracing::error!("decode custom_message failed: {}", e);
            return;
        }
        let msg = msg.unwrap();

        let result = match msg.message_type {
            MessageType::SimpleText => {
                self.text_endpoint
                    .handle_message(handler, ctx, &relay, &msg)
                    .await
            }
            MessageType::IpfsRequest => {
                if let Some(ipfs_endpoint) = self.ipfs_endpoint.as_ref() {
                    ipfs_endpoint
                        .handle_message(handler, ctx, &relay, &msg)
                        .await
                } else {
                    return;
                }
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
