#![warn(missing_docs)]
//! FFI backend behaviour implementation
//! =================================
//！
use std::ffi::c_char;
use std::ffi::CString;
use std::result::Result;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::runtime::Runtime;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageHandler;
use crate::error::Error;
use crate::prelude::MessagePayload;
use crate::provider::ffi::ProviderPtr;
use crate::provider::ffi::ProviderWithRuntime;
use crate::provider::Provider;

/// Context for handling backend behaviour
/// cbindgen:no-export
#[repr(C)]
#[derive(Clone)]
pub struct FFIBackendBehaviour {
    paintext_message_handler: Option<
        Box<
            extern "C" fn(
                *const FFIBackendBehaviourWithRuntime,
                *const ProviderPtr,
                *const c_char,
                *const c_char,
            ) -> (),
        >,
    >,
    service_message_handler: Option<
        Box<
            extern "C" fn(
                *const FFIBackendBehaviourWithRuntime,
                *const ProviderPtr,
                *const c_char,
                *const c_char,
            ) -> (),
        >,
    >,
    extension_message_handler: Option<
        Box<
            extern "C" fn(
                *const FFIBackendBehaviourWithRuntime,
                *const ProviderPtr,
                *const c_char,
                *const c_char,
            ) -> (),
        >,
    >,
}

/// A wrapper for FFIbackendbehaviour, we needs runtime to make async request work
/// cbindgen:field-names=[]
#[derive(Clone)]
pub struct FFIBackendBehaviourWithRuntime {
    behaviour: FFIBackendBehaviour,
    runtime: Arc<Runtime>,
}

macro_rules! handle_backend_message {
    ($self:ident, $provider:ident, $handler:ident, $payload: ident, $message:ident) => {
        if let Some(handler) = &$self.behaviour.$handler {
            let rt = $self.runtime.clone();

            let provider_with_runtime = ProviderWithRuntime::new($provider.clone(), rt.clone());
            provider_with_runtime.check_arc();
            let provider_ptr: ProviderPtr = (&provider_with_runtime).into();
            let payload = serde_json::to_string(&$payload)?;
            let message = serde_json::to_string(&$message)?;
            let payload = CString::new(payload)?;
            let message = CString::new(message)?;
            handler(
                $self as *const FFIBackendBehaviourWithRuntime,
                &provider_ptr as *const ProviderPtr,
                payload.as_ptr(),
                message.as_ptr(),
            );
        }
    };
}

impl FFIBackendBehaviourWithRuntime {
    /// Create a new instance
    pub fn new(behaviour: FFIBackendBehaviour, runtime: Arc<Runtime>) -> Self {
        Self {
            behaviour: behaviour.clone(),
            runtime: runtime.clone(),
        }
    }

    async fn do_handle_message(
        &self,
        provider: Arc<Provider>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<(), Error> {
        match msg {
            BackendMessage::PlainText(m) => {
                handle_backend_message!(self, provider, paintext_message_handler, payload, m)
            }
            BackendMessage::Extension(m) => {
                handle_backend_message!(self, provider, extension_message_handler, payload, m)
            }
            BackendMessage::ServiceMessage(m) => {
                handle_backend_message!(self, provider, service_message_handler, payload, m)
            }
            _ => (),
        }
        Ok(())
    }
}

/// Backend behaviour for FFI
#[no_mangle]
pub extern "C" fn new_ffi_backend_behaviour(
    paintext_message_handler: Option<
        extern "C" fn(
            *const FFIBackendBehaviourWithRuntime,
            *const ProviderPtr,
            *const c_char,
            *const c_char,
        ) -> (),
    >,
    service_message_handler: Option<
        extern "C" fn(
            *const FFIBackendBehaviourWithRuntime,
            *const ProviderPtr,
            *const c_char,
            *const c_char,
        ) -> (),
    >,
    extension_message_handler: Option<
        extern "C" fn(
            *const FFIBackendBehaviourWithRuntime,
            *const ProviderPtr,
            *const c_char,
            *const c_char,
        ) -> (),
    >,
) -> FFIBackendBehaviour {
    FFIBackendBehaviour {
        paintext_message_handler: paintext_message_handler.map(Box::new),
        service_message_handler: service_message_handler.map(Box::new),
        extension_message_handler: extension_message_handler.map(Box::new),
    }
}

#[async_trait]
impl MessageHandler<BackendMessage> for FFIBackendBehaviourWithRuntime {
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
