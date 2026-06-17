#![warn(missing_docs)]

//! Backend Message Types.
use std::sync::Arc;

use rings_core::message::MessagePayload;
use rings_rpc::protos::rings_node::SendBackendMessageRequest;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::provider::Provider;

#[cfg(feature = "snark")]
pub mod snark;

/// BackendMessage struct for handling CustomMessage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BackendMessage {
    /// Plain text
    PlainText(String),
    /// SNARK with curve pallas and vesta
    #[cfg(feature = "snark")]
    SNARKTaskMessage(snark::SNARKTaskMessage),
    /// Namespaced extension envelope, routed by the [`Extensions`] registry.
    /// Transitional: built-in variants above migrate to this and are then removed.
    ///
    /// [`Extensions`]: crate::backend::ext::Extensions
    Envelope(crate::backend::ext::Envelope),
}

/// MessageHandler trait
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait MessageHandler<T> {
    /// handle_message
    async fn handle_message(
        &self,
        provider: Arc<Provider>,
        ctx: &MessagePayload,
        data: &T,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

/// This macro is aims to generate code like
/// '''
/// impl <T1, T2, T3> MessageHandler<BackendMessage> for (T1, T2, T3)
/// where
///     T1: MessageHandler<BackendMessage> + Send + Sync + Sized,
///     T2: MessageHandler<BackendMessage> + Send + Sync + Sized,
///     T3: MessageHandler<BackendMessage> + Send + Sync + Sized,
/// {
///     async fn handle_message(
///         &self,
///         provider: Arc<Provider>,
///         ctx: &MessagePayload,
///         msg: &BackendMessage,
///     ) -> std::result::Result<(), Box<dyn std::error::Error>> {
///         self.0.handle_message(provider.clone(), ctx, msg).await?;
///         self.1.handle_message(provider.clone(), ctx, msg).await?;
///         self.2.handle_message(provider.clone(), ctx, msg).await?;
///         Ok(())
///     }
/// }
/// '''ignore
macro_rules! impl_message_handler_for_tuple {
    // Case for WebAssembly target (`wasm`)
    ($($T:ident),+; $($n: tt),+; wasm) => {
        #[async_trait::async_trait(?Send)]
        impl<$($T: MessageHandler<BackendMessage>),+> MessageHandler<BackendMessage> for ($($T),+)
        {
            async fn handle_message(
                &self,
                provider: Arc<Provider>,
                ctx: &MessagePayload,
                msg: &BackendMessage,
            ) -> std::result::Result<(), Box<dyn std::error::Error>> {
                $(
                    self.$n.handle_message(provider.clone(), ctx, msg).await?;
                )+
                Ok(())
            }
        }
    };

    // Case for non-WebAssembly targets
    ($($T:ident),+; $($n: tt),+; non_wasm) => {
        #[async_trait::async_trait]
        impl<$($T: MessageHandler<BackendMessage> + Send + Sync),+> MessageHandler<BackendMessage> for ($($T),+)
        {
            async fn handle_message(
                &self,
                provider: Arc<Provider>,
                ctx: &MessagePayload,
                msg: &BackendMessage,
            ) -> std::result::Result<(), Box<dyn std::error::Error>> {
                $(
                    self.$n.handle_message(provider.clone(), ctx, msg).await?;
                )+
                Ok(())
            }
        }
    };
}

#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2; 0, 1; non_wasm);
#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2, T3; 0, 1, 2; non_wasm);
#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2, T3, T4; 0, 1, 2, 3; non_wasm);
#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2, T3, T4, T5; 0, 1, 2, 3, 4; non_wasm);

#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2; 0, 1; wasm);
#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2, T3; 0, 1, 2; wasm);
#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2, T3, T4; 0, 1, 2, 3; wasm);
#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2, T3, T4, T5; 0, 1, 2, 3, 4; wasm);

impl BackendMessage {
    /// Convert to SendBackendMessageRequest
    pub fn into_send_backend_message_request(
        self,
        destination_did: impl ToString,
    ) -> Result<SendBackendMessageRequest, Error> {
        Ok(SendBackendMessageRequest {
            destination_did: destination_did.to_string(),
            data: serde_json::to_string(&self)?,
        })
    }
}
