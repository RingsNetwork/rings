use async_trait::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use wasm_bindgen::JsValue;

use crate::error::Error;
use crate::error::Result;
use crate::types::backend::BackendMessage;

/// MessageCallback instance for Browser
#[wasm_export]
pub struct Backend {
    backend_message_handler: js_sys::Function,
}

#[wasm_export]
impl Backend {
    /// Create a new instance of message callback, this function accept one argument:
    ///
    /// * backend_message_handler: function(payload: string, message: string) -> ();
    #[wasm_bindgen(constructor)]
    pub fn new(backend_message_handler: js_sys::Function) -> Backend {
        Backend {
            backend_message_handler,
        }
    }
}

#[async_trait(?Send)]
impl SwarmCallback for Backend {
    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let data: Message = payload.transaction.data()?;

        let Message::CustomMessage(CustomMessage(msg)) = data else {
            return Ok(());
        };

        let backend_msg = bincode::deserialize(&msg)?;
        tracing::debug!("backend_message received: {backend_msg:?}");

        self.handle_backend_message(payload, &backend_msg).await?;

        Ok(())
    }
}

impl Backend {
    async fn handle_backend_message(
        &self,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
        let this = JsValue::null();
        let r = self
            .backend_message_handler
            .call2(
                &this,
                &js_value::serialize(&payload)?,
                &js_value::serialize(&msg)?,
            )
            .map_err(|e| Error::JsError(format!("call backend_message_handler failed: {e:?}")))?;

        let p = js_sys::Promise::try_from(r).map_err(|e| {
            Error::JsError(format!(
                "convert backend_message_handler promise failed: {e:?}"
            ))
        })?;

        wasm_bindgen_futures::JsFuture::from(p).await.map_err(|e| {
            Error::JsError(format!(
                "invoke backend_message_handler promise failed: {e:?}"
            ))
        })?;

        Ok(())
    }
}
