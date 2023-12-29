#![warn(missing_docs)]
//! FFI backend behaviour implementation
//! =================================
//！
use std::ffi::c_char;
use std::ffi::CString;
use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageHandler;
use crate::error::Result;
use crate::prelude::MessagePayload;
use crate::provider::ffi::ProviderPtr;
use crate::provider::Provider;

/// Context for handling backend behaviour
#[repr(C)]
#[derive(Clone)]
pub struct FFIBackendBehaviour {
    paintext_message_handler: Option<
        Box<
            extern "C" fn(
                *const FFIBackendBehaviour,
                *const ProviderPtr,
                *const c_char,
                *const c_char,
            ) -> (),
        >,
    >,
    service_message_handler: Option<
        Box<
            extern "C" fn(
                *const FFIBackendBehaviour,
                *const ProviderPtr,
                *const c_char,
                *const c_char,
            ) -> (),
        >,
    >,
    extension_message_handler: Option<
        Box<
            extern "C" fn(
                *const FFIBackendBehaviour,
                *const ProviderPtr,
                *const c_char,
                *const c_char,
            ) -> (),
        >,
    >,
}

/// Backend behaviour for FFI
#[no_mangle]
pub extern "C" fn new_ffi_backend_behaviour(
    paintext_message_handler: Option<
        extern "C" fn(
            *const FFIBackendBehaviour,
            *const ProviderPtr,
            *const c_char,
            *const c_char,
        ) -> (),
    >,
    service_message_handler: Option<
        extern "C" fn(
            *const FFIBackendBehaviour,
            *const ProviderPtr,
            *const c_char,
            *const c_char,
        ) -> (),
    >,
    extension_message_handler: Option<
        extern "C" fn(
            *const FFIBackendBehaviour,
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

macro_rules! handle_backend_message {
    ($self:ident, $provider:ident, $handler:ident, $payload: ident, $message:ident) => {
        if let Some(handler) = &$self.$handler {
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
impl MessageHandler<BackendMessage> for FFIBackendBehaviour {
    async fn handle_message(
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
