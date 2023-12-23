#![warn(missing_docs)]
//! FFI backend behaviour implementation
//! =================================
//！
use std::ffi::c_char;
use std::ffi::CString;
use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageEndpoint;
use crate::error::Result;
use crate::prelude::MessagePayload;
use crate::provider::ffi::ProviderPtr;
use crate::provider::Provider;

/// A ffi type alice presenting fn(FFIbackendbehaviour, Provider, MessagePayload, BackendMessage)
pub type FFIBackendBehaviourHandlerFn = extern "C" fn(
    *const FFIBackendBehaviour,
    *const ProviderPtr,
    *const c_char,
    *const c_char,
) -> ();

/// Context for handling backend behaviour
#[repr(C)]
#[derive(Clone)]
pub struct FFIBackendBehaviour {
    paintext_message_handler: Option<FFIBackendBehaviourHandlerFn>,
    service_message_handler: Option<FFIBackendBehaviourHandlerFn>,
    extension_message_handler: Option<FFIBackendBehaviourHandlerFn>,
}

/// Backend behaviour for FFI
#[no_mangle]
pub extern "C" fn new_ffi_backend_behaviour(
    paintext_message_handler: Option<FFIBackendBehaviourHandlerFn>,
    service_message_handler: Option<FFIBackendBehaviourHandlerFn>,
    extension_message_handler: Option<FFIBackendBehaviourHandlerFn>,
) -> FFIBackendBehaviour {
    FFIBackendBehaviour {
        paintext_message_handler,
        service_message_handler,
        extension_message_handler,
    }
}

macro_rules! handle_backend_message {
    ($self:ident, $provider:ident, $handler:ident, $payload: ident, $message:ident) => {
        if let Some(handler) = $self.$handler {
            let provider: &Provider = Arc::as_ref(&$provider);
            let provider_ptr: ProviderPtr = provider.into();
            let payload = serde_json::to_string(&$payload)?;
            let message = serde_json::to_string(&$message)?;
            let payload = CString::new(payload)?;
            let message = CString::new(message)?;
            handler(
                $self as *const FFIBackendBehaviour,
                &provider_ptr as *const ProviderPtr,
                payload.as_ptr(),
                message.as_ptr(),
            );
        }
    };
}

#[async_trait]
impl MessageEndpoint<BackendMessage> for FFIBackendBehaviour {
    async fn on_message(
        &self,
        provider: Arc<Provider>,
        payload: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<()> {
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
        }
        Ok(())
    }
}
