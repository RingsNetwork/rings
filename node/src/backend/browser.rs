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

/// MessageCallback instance for Browser
#[wasm_export]
#[derive(Clone)]
pub struct BackendContext {
    backend_message_handler: Function,
}

#[wasm_export]
impl BackendContext {
    /// Create a new instance of message callback, this function accept one argument:
    ///
    /// * backend_message_handler: `function(provider: Arc<Provider>, payload: string, message: string) -> Promise<()>`;
    #[wasm_bindgen(constructor)]
    pub fn new(backend_message_handler: js_sys::Function) -> BackendContext {
        BackendContext {
            backend_message_handler,
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
        // let _ =
        //     js_handler_wrapper(&self.backend_message_handler)(&self, provider, payload, msg).await;
        let provider = provider.clone().as_ref().clone();
        let ctx = js_value::serialize(&payload)?.clone();
        let msg = js_value::serialize(&msg)?.clone();

        let _ = js_func::of4::<BackendContext, Provider, JsValue, JsValue>(
            &self.backend_message_handler,
        )(self, &provider, &ctx, &msg)
        .await;
        Ok(())
    }
}
