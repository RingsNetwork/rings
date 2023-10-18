//! ffi

use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;
use std::sync::Arc;

use futures::executor;
use rings_core::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessageCallback;
use rings_core::message::MessageHandlerEvent;
use rings_core::message::MessagePayload;

use super::Client;
use crate::error::Result;
use crate::jsonrpc::HandlerType;
use crate::processor::Processor;

#[repr(C)]
pub struct ClientPtr {
    processor: *const Processor,
    handler: *const HandlerType,
}

impl ClientPtr {
    pub unsafe fn increase_strong_count(&self) -> () {
        Arc::increment_strong_count(&self.processor);
        Arc::increment_strong_count(&self.handler);
    }

    pub unsafe fn decrease_strong_count(&self) -> () {
        Arc::decrement_strong_count(&self.processor);
        Arc::decrement_strong_count(&self.handler);
    }
}

impl Client {
    pub unsafe fn from_ptr(ptr: &ClientPtr) -> Client {
        // We create Arcs from raw pointers but we don't clone these Arcs
        // since it will create unnecessary increments in the ref count.
        let processor = unsafe { Arc::<Processor>::from_raw(ptr.processor as *const Processor) };
        let handler = unsafe { Arc::<HandlerType>::from_raw(ptr.handler as *const HandlerType) };

        // avoid double release
        let _ = Arc::into_raw(processor.clone());
        let _ = Arc::into_raw(handler.clone());

        Self { processor, handler }
    }

    pub fn into_ptr(&self) -> ClientPtr {
        // Clone the Arcs, which increases the ref count,
        // then turn them into raw pointers. This makes sure the memory
        // won't be deallocated as long as we properly manage the raw pointers.
        let processor_ptr = Arc::into_raw(self.processor.clone());
        let handler_ptr = Arc::into_raw(self.handler.clone());

        unsafe {
            Arc::increment_strong_count(&processor_ptr);
            Arc::increment_strong_count(&handler_ptr);
        }

        ClientPtr {
            processor: processor_ptr,
            handler: handler_ptr,
        }
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MessageCallback for MessageCallbackInstanceFFI {
    async fn custom_message(
        &self,
        relay: &MessagePayload<Message>,
        msg: &CustomMessage,
    ) -> Vec<MessageHandlerEvent> {
        match (|| -> Result<()> {
            let relay = serde_json::to_string(relay)?;
            let msg = serde_json::to_string(msg)?;
            if let Some(cb) = self.custom_message_callback {
                let _relay = CString::new(relay)?;
                let _msg = CString::new(msg)?;
                cb(_relay.as_ptr(), _msg.as_ptr())
            };
            Ok(())
        })() {
            Ok(()) => (),
            Err(e) => {
                log::error!("Failed on handle builtin_message {:#}", e);
            }
        }
        vec![]
    }

    async fn builtin_message(&self, relay: &MessagePayload<Message>) -> Vec<MessageHandlerEvent> {
        match (|| -> Result<()> {
            let relay = serde_json::to_string(relay)?;
            if let Some(cb) = self.builtin_message_callback {
                let _relay = CString::new(relay)?;
                cb(_relay.as_ptr())
            };
            Ok(())
        })() {
            Ok(()) => (),
            Err(e) => {
                log::error!("Failed on handle builtin_message {:#}", e);
            }
        };
        vec![]
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct MessageCallbackInstanceFFI {
    custom_message_callback: Option<extern "C" fn(*const c_char, *const c_char) -> ()>,
    builtin_message_callback: Option<extern "C" fn(*const c_char) -> ()>,
}

#[cfg(not(feature = "browser"))]
unsafe impl Send for MessageCallbackInstanceFFI {}
#[cfg(not(feature = "browser"))]
unsafe impl Sync for MessageCallbackInstanceFFI {}

#[no_mangle]
pub extern "C" fn new_callback(
    custom_message_cb: Option<extern "C" fn(*const c_char, *const c_char) -> ()>,
    builtin_message_cb: Option<extern "C" fn(*const c_char) -> ()>,
) -> MessageCallbackInstanceFFI {
    MessageCallbackInstanceFFI {
        custom_message_callback: custom_message_cb,
        builtin_message_callback: builtin_message_cb,
    }
}

#[no_mangle]
pub unsafe extern "C" fn listen(client_ptr: *const ClientPtr) {
    let client_ptr: &ClientPtr = unsafe { &*client_ptr };
    let client: Client = unsafe { Client::from_ptr(client_ptr) };
    let p = client.processor.clone();
    executor::block_on(p.listen());
}

#[no_mangle]
pub unsafe extern "C" fn request(
    client: &ClientPtr,
    method: *const c_char,
    request: *const c_char,
) -> *const c_char {
    match (|| -> Result<*const c_char> {
        let client: Client = unsafe { Client::from_ptr(client) };
        let c_method = unsafe { CStr::from_ptr(method) };
        let c_request = unsafe { CStr::from_ptr(request) };
        let method = c_method.to_str()?.to_owned();
        let request = c_request.to_str()?.to_owned();
        let ret: String = executor::block_on(client.request_internal(method, request, None))?;
        let c_ret = CString::new(ret)?;
        Ok(c_ret.as_ptr())
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("FFI: Failed on request {:#}", e)
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn new_client_with_callback(
    ice_server: *const c_char,
    stabilize_timeout: u32,
    account: *const c_char,
    account_type: *const c_char,
    signer: extern "C" fn(*const c_char) -> *const c_char,
    callback_ptr: *const MessageCallbackInstanceFFI,
) -> ClientPtr {
    fn wrapped_signer(
        signer: extern "C" fn(*const c_char) -> *const c_char,
    ) -> impl Fn(String) -> Vec<u8> {
        move |data: String| -> Vec<u8> {
            let c_data = CString::new(data).expect("Failed on covering String to CString");
            let sig = signer(c_data.as_ptr());
            let c_ret = unsafe { CStr::from_ptr(sig) };
            c_ret.to_bytes().to_vec()
        }
    }

    let client: Client = match (|| -> Result<Client> {
        let c_ice = unsafe { CStr::from_ptr(ice_server) };
        let c_acc = unsafe { CStr::from_ptr(account) };
        let c_acc_ty = unsafe { CStr::from_ptr(account_type) };

        let ice: String = c_ice.to_str()?.to_owned();
        let acc: String = c_acc.to_str()?.to_owned();
        let acc_ty: String = c_acc_ty.to_str()?.to_owned();

        let callback: &MessageCallbackInstanceFFI = unsafe { &*callback_ptr };
        let cb: super::TyMessageCallback = Box::new(callback.clone());

        executor::block_on(Client::new_client_internal(
            ice,
            stabilize_timeout as usize,
            acc,
            acc_ty,
            Box::new(wrapped_signer(signer)),
            Some(cb),
        ))
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed on create new client {:#}", e)
        }
    };
    let ret = client.into_ptr();
    unsafe {
        ret.increase_strong_count();
    }
    ret
}
