use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Array;
use js_sys::Function;
use js_sys::JsString;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::backend::Backend;
use crate::client::Client;
use crate::error::Error;
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

        let _ = js_func_wrapper_of_4::<BackendContext, Client, JsValue, JsValue>(
            &self.backend_message_handler,
        )(
            &self,
	    &client,
	    &ctx,
	    &msg
        )
        .await;
        Ok(())
    }
}


/// A Wrapper for js_sys::Function with type
pub fn js_func_wrapper_of_4<'a, 'b: 'a, T0: Into<JsValue> + Clone, T1: Into<JsValue> + Clone, T2: Into<JsValue> + Clone, T3: Into<JsValue> + Clone, F: From<JsValue>>(
    func: &Function,
) -> Box<
    dyn Fn(&'b T0, &'b T1, &'b T2, &'b T3) -> Pin<Box<dyn Future<Output = F> + 'b>>,
> {
    let func = func.clone();
    Box::new(
        move |a: &T0, b: &T1, c: &T2, d: &T3| -> Pin<Box<dyn Future<Output = F>>> {
            let func = func.clone();
            Box::pin(async move {
                let func = func.clone();
                JsFuture::from(js_sys::Promise::from(
                    func.apply(
                        &JsValue::NULL,
                        &Array::of4(
                            &a.clone().into(),
                            &b.clone().into(),
                            &c.clone().into(),
                            &d.clone().into(),
                        ),
                    )
                    .map_err(|e| Error::JsError(js_sys::Error::from(e).to_string().into()))?,
                ))
                .await
                .map_err(|e| Error::JsError(js_sys::Error::from(e).to_string().into()))?;
                Ok(())
            })
        },
    )
}
