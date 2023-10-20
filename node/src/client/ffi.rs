#![warn(missing_docs)]
//! ffi Client implementation
//! =======================
//! This module allows developers to integrate the client with various programming languages,
//! such as C, C++, Golang, Python, and Node.js.
//!
//! The module provides functionality for integrating Rust-based systems with external
//! systems through FFI (Foreign Function Interface). This is particularly useful when
//! other programming languages want to interface with the functionalities provided by
//! this Rust module.
//!
//! Primary Features:
//! 1. **Client Representation for FFI**: The module defines `ClientPtr`, a struct that
//!    serves as a C-compatible representation of the `Client` type, allowing for interaction
//!    with other languages through raw pointers. It abstracts the reference counting of
//!    internal `Arc` components, ensuring memory safety across the boundary.
//!
//! 2. **Message Callback for FFI**: The `MessageCallbackInstanceFFI` struct serves as a bridge
//!    for message callback functionalities between Rust and other languages. It can hold
//!    function pointers to C-compatible functions that handle custom and built-in messages.
//!
//! 3. **Functions for Client Interaction**: Several extern "C" functions, such as `new_client_with_callback`,
//!    `listen`, and `async_listen`, facilitate the creation of clients, listening to messages,
//!    and making internal requests. They make the module's core functionalities accessible from C
//!    or other languages supporting FFI.
//!
//! This FFI integration is essential when this Rust module is part of a larger system, which might be
//! written in different languages, and needs a standardized way to communicate with or make use of
//! functionalities offered by Rust.
//!
//! Note: As with all FFI interactions, special care must be taken regarding memory safety. Functions
//! and methods marked with `# Safety` in this module require the caller to ensure specific invariants
//! for safe operation.
//!
//! # Examples
//!
//! Here's an example of how to use the Rings FFI with Python.
//!
//! # Example with Python
//! ```python
//! import cffi
//! import ctypes
//! from web3 import Web3;
//! from eth_account.messages import encode_defunct
//! import re
//! w3 = Web3();
//! acc = w3.eth.account.create();
//! ffi = cffi.FFI()
//! c_header = open("./node/bindings.h", "r").read()
//! c_header = re.sub(r'#define .*', '', c_header)
//! ffi.cdef(c_header)
//! rings_node = ffi.dlopen("./target/debug/librings_node.dylib")
//!
//! @ffi.callback("char*(*)(char *)")
//! def signer(msg):
//!    c_input = ffi.string(msg)
//!    decoded = encode_defunct(c_input)
//!    sig = acc1.sign_message(decoded)
//!    ret = bytes(sig.signature)
//!    return ffi.new("char[]", ret)
//!
//! @ffi.callback("void(*)(char *, char *)")
//! def custom_msg_callback(msg):
//!     print(msg)
//!     return
//!
//! @ffi.callback("void(*)(char *)")
//! def builtin_msg_callback(msg):
//!    print(msg)
//!    return

//! rings_node.init_logging(rings_node.Debug)
//! callback = rings_node.new_callback(custom_msg_callback, builtin_msg_callback)
//!
//! def create_client(signer, acc):
//!     client = rings_node.new_client_with_callback(
//!         "stun://stun.l.google.com".encode(),
//!         10,
//!         acc.address.encode(),
//!         "eip191".encode(),
//!         signer,
//!         ffi.addressof(callback)
//!     )
//!     return client
//!
//! if __name__ == "__main__":
//!     client = create_client(signer, acc1)
//!     rings_node.listen(ffi.addressof(client))
//!     print(client)
//! ```
//! 
//! Note: Since the above code is executed in a single-process environment of Python,
//! the Rings' listen loop will block the process. If you wish to use it in a production environment,
//! you should implement your own more advanced process or thread management.

use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;
use std::sync::Arc;

use futures::executor;
use rings_core::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::MessageCallback;
use rings_core::message::MessageHandlerEvent;
use rings_core::message::MessagePayload;

use super::Client;
use super::Signer;
use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::HandlerType;
use crate::prelude::CallbackFn;
use crate::processor::Processor;

/// A structure to represent the Client in a C-compatible format.
/// This is necessary as using Arc directly in FFI can be unsafe.
#[repr(C)]
pub struct ClientPtr {
    processor: *const Processor,
    handler: *const HandlerType,
}

impl ClientPtr {
    /// Increases the reference count for the associated Arcs.
    /// # Safety
    /// This function unsafely increments the reference count of the Arcs.
    pub unsafe fn increase_strong_count(&self) {
        Arc::increment_strong_count(&self.processor);
        Arc::increment_strong_count(&self.handler);
    }

    /// Decreases the reference count for the associated Arcs.
    /// # Safety
    /// This function unsafely decrements the reference count of the Arcs.
    pub unsafe fn decrease_strong_count(&self) {
        Arc::decrement_strong_count(&self.processor);
        Arc::decrement_strong_count(&self.handler);
    }
}

impl std::ops::Drop for ClientPtr {
    fn drop(&mut self) {
        tracing::debug!("Ptr dropped!");
        unsafe {
            self.decrease_strong_count();
        }
    }
}

impl Client {
    /// Converts a raw ClientPtr pointer to a Rust Client type.
    /// # Safety
    /// Unsafe due to the dereferencing of the raw pointer.
    fn from_raw(ptr: *const ClientPtr) -> Result<Client> {
        // Check point here.
        if ptr.is_null() {
            return Err(Error::FFINulPtrError);
        }

        let client_ptr: &ClientPtr = unsafe { &*ptr };
        let client: Client = client_ptr.into();
        Ok(client)
    }
}

impl From<&ClientPtr> for Client {
    /// Converts a reference to a ClientPtr to a Client type.
    /// Note that the conversion from raw pointers to Arcs does not modify the reference count.
    /// # Safety
    /// Unsafe due to the conversion from raw pointers to Arcs.
    fn from(ptr: &ClientPtr) -> Client {
        tracing::debug!("FFI: Client from Ptr!");
        let processor = unsafe { Arc::<Processor>::from_raw(ptr.processor as *const Processor) };
        let handler = unsafe { Arc::<HandlerType>::from_raw(ptr.handler as *const HandlerType) };
        Self { processor, handler }
    }
}

impl From<&Client> for ClientPtr {
    /// Cast a Client into ClientPtr
    fn from(client: &Client) -> ClientPtr {
        tracing::debug!("FFI: Client into Ptr!");
        // Clone the Arcs, which increases the ref count,
        // then turn them into raw pointers.
        let processor_ptr = Arc::into_raw(client.processor.clone());
        let handler_ptr = Arc::into_raw(client.handler.clone());
        let ret = ClientPtr {
            processor: processor_ptr,
            handler: handler_ptr,
        };
        ret
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MessageCallback for MessageCallbackInstanceFFI {
    async fn custom_message(
        &self,
        relay: &MessagePayload,
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

    async fn builtin_message(&self, relay: &MessagePayload) -> Vec<MessageHandlerEvent> {
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

/// The MessageCallback Instance for FFI,
/// This struct holding two functions `custom_message_callback` and `builtin_message_callback`
#[repr(C)]
#[derive(Clone)]
pub struct MessageCallbackInstanceFFI {
    custom_message_callback: Option<extern "C" fn(*const c_char, *const c_char) -> ()>,
    builtin_message_callback: Option<extern "C" fn(*const c_char) -> ()>,
}

/// Create a neww callback instance
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

/// Start message listening and stabilization
/// # Safety
/// Listen function accept a ClientPtr and will unsafety cast it into Arc based Client
#[no_mangle]
pub extern "C" fn listen(client_ptr: *const ClientPtr) {
    let client: Client = Client::from_raw(client_ptr).expect("Client ptr is invalid");
    let processor = client.processor.clone();
    executor::block_on(processor.listen());
}

/// Start message listening and stabilization
/// This function will launch listener in a new thread
/// # Safety
/// Listen function accept a ClientPtr and will unsafety cast it into Arc based Client
#[no_mangle]
pub extern "C" fn async_listen(client_ptr: *const ClientPtr) {
    let client: Client = Client::from_raw(client_ptr).expect("Client ptr is invalid");
    let processor = client.processor.clone();
    std::thread::spawn(move || {
        executor::block_on(processor.listen());
    });
}

/// Request internal rpc api
/// # Safety
///
/// * This function accept a ClientPtr and will unsafety cast it into Arc based Client
/// * This function cast CStr into Str
#[no_mangle]
pub extern "C" fn request(
    client_ptr: *const ClientPtr,
    method: *const c_char,
    params: *const c_char,
) -> *const c_char {
    match (|| -> Result<*const c_char> {
        let client: Client = Client::from_raw(client_ptr)?;
        let method = c_char_to_string(method)?;
        let params = c_char_to_string(params)?;
        let params = serde_json::from_str(&params)?;
        let ret: String = serde_json::to_string(&executor::block_on(
            client.request_internal(method, params, None),
        )?)?;
        let c_ret = CString::new(ret)?;
        Ok(c_ret.as_ptr())
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("FFI: Failed on request {:#}", e)
        }
    }
}

/// Craft a new Client with signer and callback ptr
/// # Safety
///
/// * This function cast CStr into Str
#[no_mangle]
pub extern "C" fn new_client_with_callback(
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
            let c_ret = c_char_to_bytes(sig).expect("Failed on covering c char to [u8]");
            c_ret
        }
    }

    let client: Client = match (|| -> Result<Client> {
        let ice: String = c_char_to_string(ice_server)?;
        let acc: String = c_char_to_string(account)?;
        let acc_ty: String = c_char_to_string(account_type)?;

        let callback: &MessageCallbackInstanceFFI = unsafe { &*callback_ptr };
        let cb: CallbackFn = Box::new(callback.clone());

        executor::block_on(Client::new_client_internal(
            ice,
            stabilize_timeout as usize,
            acc,
            acc_ty,
            Signer::Sync(Box::new(wrapped_signer(signer))),
            Some(cb),
        ))
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed on create new client {:#}", e)
        }
    };
    let ret: ClientPtr = (&client).into();
    // When leaving the closure, the origin Client ref will be release,
    // So we manually increase the count here
    unsafe {
        ret.increase_strong_count();
    }
    ret
}

fn c_char_to_string(ptr: *const c_char) -> Result<String> {
    let c_str = c_char_to_bytes(ptr);
    // Drop none utf8 sym here.
    Ok(String::from_utf8(c_str?).map_err(Error::FFIFromUtf8Error)?)
}

fn c_char_to_bytes(ptr: *const c_char) -> Result<Vec<u8>> {
    // Check point here.
    if ptr.is_null() {
        return Err(Error::FFINulPtrError);
    }
    // Drop none utf8 sym here.
    let c_str: &CStr = unsafe { CStr::from_ptr(ptr) };
    Ok(c_str.to_owned().into())
}
