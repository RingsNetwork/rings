#![warn(missing_docs)]
//! This module provide basic mechanism.

pub mod ext;
pub mod protocols;
#[cfg(feature = "snark")]
pub mod snark;
pub mod transport;
pub mod types;
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use rings_core::swarm::callback::SwarmCallback;
use rings_derive::wasm_export;

use crate::backend::ext::Extensions;
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
    extensions: Extensions,
}

impl Backend {
    /// Create a new backend instance with Provider and Handler functions.
    ///
    /// The extension registry is shared with the provider, so protocols
    /// registered via the provider are visible to inbound dispatch here.
    pub fn new(provider: Arc<Provider>, handler: Box<HandlerTrait>) -> Self {
        let extensions = provider.extensions();
        Self {
            provider,
            handler,
            extensions,
        }
    }

    async fn on_backend_message(
        &self,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // New namespaced path: route envelopes to the protocol registry. The
        // interpreter is the only side-effecting boundary.
        if let BackendMessage::Envelope(envelope) = msg {
            let from = payload.transaction.signer();
            let interpreter = self.provider.interpreter();
            self.extensions
                .dispatch(&interpreter, from, envelope.clone())
                .await?;
            return Ok(());
        }

        // Legacy path (built-in variants), removed once all are ported.
        let provider = self.provider.clone();
        self.handler.handle_message(provider, payload, msg).await
    }
}

/// This struct is used to simulate `impl T`
/// We need this structure because wasm_bindgen does not support general type such as
/// `dyn T` or `impl T`
/// We use Arc instead Box, to make it cloneable.
#[wasm_export]
pub struct BackendMessageHandlerDynObj {
    #[allow(dead_code)]
    inner: Arc<HandlerTrait>,
}

impl BackendMessageHandlerDynObj {
    /// create new instance
    #[cfg(not(feature = "browser"))]
    pub fn new<T: MessageHandler<BackendMessage> + Send + Sync + 'static>(a: Arc<T>) -> Self {
        Self { inner: a.clone() }
    }

    /// create new instance
    #[cfg(feature = "browser")]
    pub fn new<T: MessageHandler<BackendMessage> + 'static>(a: Arc<T>) -> Self {
        Self { inner: a.clone() }
    }
}

impl From<BackendMessageHandlerDynObj> for Arc<dyn MessageHandler<BackendMessage>> {
    fn from(impl_backend: BackendMessageHandlerDynObj) -> Arc<dyn MessageHandler<BackendMessage>> {
        impl_backend.inner
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MessageHandler<BackendMessage> for BackendMessageHandlerDynObj {
    async fn handle_message(
        &self,
        provider: Arc<Provider>,
        ctx: &MessagePayload,
        msg: &BackendMessage,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.handle_message(provider.clone(), ctx, msg).await?;
        Ok(())
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
