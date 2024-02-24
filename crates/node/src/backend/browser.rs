#![warn(missing_docs)]
//! BackendBehaviour implementation for browser
use core::cell::RefCell;
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Function;
use rings_core::message::MessagePayload;
use rings_core::utils::js_func;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use wasm_bindgen::JsValue;

use super::BackendMessageHandlerDynObj;
use crate::backend::types::BackendMessage;
use crate::backend::types::MessageHandler;
use crate::error::Error;
use crate::provider::Provider;

/// BackendBehaviour is a context instance for handling backend message for browser
#[wasm_export]
#[derive(Clone)]
pub struct BackendBehaviour {
    handlers: dashmap::DashMap<String, Function>,
    extend_handler: RefCell<Option<Arc<dyn MessageHandler<BackendMessage>>>>,
}

#[wasm_export]
impl BackendBehaviour {
    /// Create a new instance of message callback, this function accept one argument:
    #[allow(clippy::new_without_default)]
    #[wasm_bindgen(constructor)]
    pub fn new() -> BackendBehaviour {
        BackendBehaviour {
            handlers: dashmap::DashMap::<String, Function>::new(),
            extend_handler: RefCell::new(None),
        }
    }

    /// Get behaviour as dyn obj ref
    pub fn as_dyn_obj(self) -> BackendMessageHandlerDynObj {
        BackendMessageHandlerDynObj::new(Arc::new(self))
    }

    /// Extend backend with other backend
    pub fn extend(self, impl_backend: BackendMessageHandlerDynObj) {
        self.extend_handler.replace(Some(impl_backend.into()));
    }

    /// register call back function
    /// * func: `function(provider: Arc<Provider>, payload: string, message: string) -> Promise<()>`;
    pub fn on(&self, method: String, func: js_sys::Function) {
        self.handlers.insert(method, func);
    }

    fn get_handler(&self, method: &str) -> Option<js_sys::Function> {
        self.handlers.get(method).map(|v| v.value().clone())
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
                if let Some(func) = &self.get_handler("ServiceMessage") {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider.clone(), ctx, m).await?;
                }
            }
            BackendMessage::Extension(m) => {
                if let Some(func) = &self.get_handler("Extension") {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider.clone(), ctx, m).await?;
                }
            }
            BackendMessage::PlainText(m) => {
                if let Some(func) = &self.get_handler("PlainText") {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider.clone(), ctx, m).await?;
                }
            }
            #[cfg(feature = "snark")]
            BackendMessage::SNARKTaskMessage(m) => {
                if let Some(func) = &self.get_handler("SNARKTaskMessage") {
                    let m = js_value::serialize(m)?;
                    let cb = js_func::of4::<BackendBehaviour, Provider, JsValue, JsValue>(func);
                    cb(self.clone(), provider.clone(), ctx, m).await?;
                }
            }
        }
        if let Some(ext) = &self.extend_handler.clone().into_inner() {
            ext.handle_message(provider.into(), payload, msg)
                .await
                .map_err(|e| Error::BackendError(e.to_string()))?;
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
