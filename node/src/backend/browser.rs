#![warn(missing_docs)]
//! BackendContext implementation for browser
use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Function;
use rings_core::message::MessagePayload;
use rings_core::utils::js_func;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use wasm_bindgen::JsValue;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::error::Result;
use crate::provider::Provider;

/// BackendContext is a context instance for handling backend message for browser
#[wasm_export]
#[derive(Clone)]
pub struct BackendContext {
    service_message_handler: Option<Function>,
    plain_text_message_handler: Option<Function>,
    extension_message_handler: Option<Function>,
}

#[wasm_export]
impl BackendContext {
    /// Create a new instance of message callback, this function accept one argument:
    ///
    /// * backend_message_handler: `function(provider: Arc<Provider>, payload: string, message: string) -> Promise<()>`;
    #[wasm_bindgen(constructor)]
    pub fn new(
        service_message_handler: Option<js_sys::Function>,
        plain_text_message_handler: Option<js_sys::Function>,
        extension_message_handler: Option<Function>,
    ) -> BackendContext {
        BackendContext {
            service_message_handler,
            plain_text_message_handler,
            extension_message_handler,
        }
    }
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
        let provider = provider.clone().as_ref().clone();
        let ctx = js_value::serialize(&payload)?.clone();
        match msg {
            BackendMessage::ServiceMessage(m) => {
                if let Some(func) = &self.service_message_handler {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendContext, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider, ctx, m).await?;
                }
            }
            BackendMessage::Extension(m) => {
                if let Some(func) = &self.extension_message_handler {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendContext, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider, ctx, m).await?;
                }
            }
            BackendMessage::PlainText(m) => {
                if let Some(func) = &self.plain_text_message_handler {
                    let cb = js_func::of4::<BackendContext, Provider, JsValue, String>(func);
                    cb(self.clone(), provider, ctx, m.to_string()).await?;
                }
            }
        }
        Ok(())
    }
}
