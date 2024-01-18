#![warn(missing_docs)]
//! BackendBehaviour implementation for browser
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Function;
use rings_core::message::MessagePayload;
use rings_core::utils::js_func;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use wasm_bindgen::JsValue;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageHandler;
use crate::error::Error;
use crate::provider::Provider;

/// BackendBehaviour is a context instance for handling backend message for browser
#[wasm_export]
#[derive(Clone)]
pub struct BackendBehaviour {
    service_message_handler: Option<Function>,
    plain_text_message_handler: Option<Function>,
    extension_message_handler: Option<Function>,
}

#[wasm_export]
impl BackendBehaviour {
    /// Create a new instance of message callback, this function accept one argument:
    ///
    /// * backend_message_handler: `function(provider: Arc<Provider>, payload: string, message: string) -> Promise<()>`;
    #[wasm_bindgen(constructor)]
    pub fn new(
        service_message_handler: Option<js_sys::Function>,
        plain_text_message_handler: Option<js_sys::Function>,
        extension_message_handler: Option<Function>,
    ) -> BackendBehaviour {
        BackendBehaviour {
            service_message_handler,
            plain_text_message_handler,
            extension_message_handler,
        }
    }

    async fn do_handle_message(
        &self,
        provider: Arc<Provider>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<(), Error> {
        let provider = provider.clone().as_ref().clone();
        let ctx = js_value::serialize(&payload)?;
        match msg {
            BackendMessage::ServiceMessage(m) => {
                if let Some(func) = &self.service_message_handler {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider, ctx, m).await?;
                }
            }
            BackendMessage::Extension(m) => {
                if let Some(func) = &self.extension_message_handler {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider, ctx, m).await?;
                }
            }
            BackendMessage::PlainText(m) => {
                if let Some(func) = &self.plain_text_message_handler {
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, String>(func);
                    cb(self.clone(), provider, ctx, m.to_string()).await?;
                }
            }
            _ => Ok(()),
        }
        Ok(())
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MessageHandler<BackendMessage> for BackendBehaviour {
    async fn handle_message(
        &self,
        provider: Arc<Provider>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.do_handle_message(provider, payload, msg)
            .await
            .map_err(|e| e.into())
    }
}
