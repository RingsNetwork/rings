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
                if let Some(handler) = self.paintext_message_handler {
                    let provider: &Provider = Arc::as_ref(&provider);
                    let provider_ptr: ProviderPtr = provider.into();
                    let payload = serde_json::to_string(&payload)?;
                    let message = serde_json::to_string(&m)?;
                    let payload = CString::new(payload)?;
                    let message = CString::new(message)?;
                    handler(
                        self as *const FFIBackendBehaviour,
                        &provider_ptr as *const ProviderPtr,
                        payload.as_ptr(),
                        message.as_ptr(),
                    );
                }
            }
            BackendMessage::Extension(m) => {
                if let Some(handler) = self.extension_message_handler {
                    let provider: &Provider = Arc::as_ref(&provider);
                    let provider_ptr: ProviderPtr = provider.into();
                    let payload = serde_json::to_string(&payload)?;
                    let message = serde_json::to_string(&m)?;
                    let payload = CString::new(payload)?;
                    let message = CString::new(message)?;
                    handler(
                        self as *const FFIBackendBehaviour,
                        &provider_ptr as *const ProviderPtr,
                        payload.as_ptr(),
                        message.as_ptr(),
                    );
                }
            }
            BackendMessage::ServiceMessage(m) => {
                if let Some(handler) = self.service_message_handler {
                    let provider: &Provider = Arc::as_ref(&provider);
                    let provider_ptr: ProviderPtr = provider.into();
                    let payload = serde_json::to_string(&payload)?;
                    let message = serde_json::to_string(&m)?;
                    let payload = CString::new(payload)?;
                    let message = CString::new(message)?;
                    handler(
                        self as *const FFIBackendBehaviour,
                        &provider_ptr as *const ProviderPtr,
                        payload.as_ptr(),
                        message.as_ptr(),
                    );
                }
            }
        }
        Ok(())
    }
}
