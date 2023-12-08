use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Function;
use rings_core::message::MessagePayload;
use rings_core::utils::js_func;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::backend::Backend;
use crate::client::Client;
use crate::error::Result;

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
    /// * backend_message_handler: function(client: Arc<Client>, payload: string, message: string) -> Promise<()>;
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
    async fn handle_message(
        &self,
        client: Arc<Client>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
        // let _ =
        //     js_handler_wrapper(&self.backend_message_handler)(&self, client, payload, msg).await;
        let client = client.clone().as_ref().clone();
        let ctx = js_value::serialize(&payload)?.clone();
        let msg = js_value::serialize(&msg)?.clone();

        let _ = js_func::of4::<BackendContext, Client, JsValue, JsValue>(
            &self.backend_message_handler,
        )(&self, &client, &ctx, &msg)
        .await;
        Ok(())
    }
}
