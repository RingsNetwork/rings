#![warn(missing_docs)]
//! ffi Provider implementation
//! =======================
//! This module allows developers to integrate the provider with various programming languages,
//! such as C, C++, Golang, Python, and Node.js.
//!
//! The module provides functionality for integrating Rust-based systems with external
//! systems through FFI (Foreign Function Interface). This is particularly useful when
//! other programming languages want to interface with the functionalities provided by
//! this Rust module.
//!
//! Primary Features:
//! 1. **Provider Representation for FFI**: The module defines `ProviderPtr`, a struct that
//!    serves as a C-compatible representation of the `Provider` type, allowing for interaction
//!    with other languages through raw pointers. It abstracts the reference counting of
//!    internal `Arc` components, ensuring memory safety across the boundary.
//!
//! 2. **Message Callback for FFI**: The `SwarmCallbackInstanceFFI` struct serves as a bridge
//!    for message callback functionalities between Rust and other languages. It can hold
//!    function pointers to C-compatible functions that handle custom and built-in messages.
//!
//! 3. **Functions for Provider Interaction**: Several extern "C" functions, such as `new_provider_with_callback`,
//!    `listen`, and `async_listen`, facilitate the creation of providers, listening to messages,
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
//!    sig = acc.sign_message(decoded)
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
//! def create_provider(signer, acc):
//!     provider = rings_node.new_provider_with_callback(
//!         "stun://stun.l.google.com".encode(),
//!         10,
//!         acc.address.encode(),
//!         "eip191".encode(),
//!         signer,
//!         ffi.addressof(callback)
//!     )
//!     return provider
//!
//! if __name__ == "__main__":
//!     provider = create_provider(signer, acc)
//!     rings_node.listen(ffi.addressof(provider))
//!     print(provider)
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

use super::Provider;
use super::Signer;
use crate::backend::ffi::FFIBackendBehaviour;
use crate::backend::Backend;
use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::HandlerType;
use crate::processor::Processor;

/// A structure to represent the Provider in a C-compatible format.
/// This is necessary as using Arc directly in FFI can be unsafe.
#[repr(C)]
pub struct ProviderPtr {
    processor: *const Processor,
    handler: *const HandlerType,
}

impl ProviderPtr {
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

impl std::ops::Drop for ProviderPtr {
    fn drop(&mut self) {
        tracing::debug!("Ptr dropped!");
        unsafe {
            self.decrease_strong_count();
        }
    }
}

impl Provider {
    /// Converts a raw ProviderPtr pointer to a Rust Provider type.
    /// # Safety
    /// Unsafe due to the dereferencing of the raw pointer.
    fn from_raw(ptr: *const ProviderPtr) -> Result<Provider> {
        // Check point here.
        if ptr.is_null() {
            return Err(Error::FFINulPtrError);
        }

        let provider_ptr: &ProviderPtr = unsafe { &*ptr };
        let provider: Provider = provider_ptr.into();
        Ok(provider)
    }
}

impl From<&ProviderPtr> for Provider {
    /// Converts a reference to a ProviderPtr to a Provider type.
    /// Note that the conversion from raw pointers to Arcs does not modify the reference count.
    /// # Safety
    /// Unsafe due to the conversion from raw pointers to Arcs.
    fn from(ptr: &ProviderPtr) -> Provider {
        tracing::debug!("FFI: Provider from Ptr!");
        let processor = unsafe { Arc::<Processor>::from_raw(ptr.processor as *const Processor) };
        let handler = unsafe { Arc::<HandlerType>::from_raw(ptr.handler as *const HandlerType) };
        Self { processor, handler }
    }
}

impl From<&Provider> for ProviderPtr {
    /// Cast a Provider into ProviderPtr
    fn from(provider: &Provider) -> ProviderPtr {
        tracing::debug!("FFI: Provider into Ptr!");
        // Clone the Arcs, which increases the ref count,
        // then turn them into raw pointers.
        let processor_ptr = Arc::into_raw(provider.processor.clone());
        let handler_ptr = Arc::into_raw(provider.handler.clone());
        ProviderPtr {
            processor: processor_ptr,
            handler: handler_ptr,
        }
    }
}

/// Start message listening and stabilization
/// # Safety
/// Listen function accept a ProviderPtr and will unsafety cast it into Arc based Provider
#[no_mangle]
pub extern "C" fn listen(provider_ptr: *const ProviderPtr) {
    let provider: Provider = Provider::from_raw(provider_ptr).expect("Provider ptr is invalid");
    executor::block_on(provider.processor.listen());
}

/// Start message listening and stabilization
/// This function will launch listener in a new thread
/// # Safety
/// Listen function accept a ProviderPtr and will unsafety cast it into Arc based Provider
#[no_mangle]
pub extern "C" fn async_listen(provider_ptr: *const ProviderPtr) {
    let provider: Provider = Provider::from_raw(provider_ptr).expect("Provider ptr is invalid");
    std::thread::spawn(move || {
        executor::block_on(provider.processor.listen());
    });
}

/// Request internal rpc api
/// # Safety
///
/// * This function accept a ProviderPtr and will unsafety cast it into Arc based Provider
/// * This function cast CStr into Str
#[no_mangle]
pub extern "C" fn request(
    provider_ptr: *const ProviderPtr,
    method: *const c_char,
    params: *const c_char,
) -> *const c_char {
    match (|| -> Result<*const c_char> {
        let provider: Provider = Provider::from_raw(provider_ptr)?;
        let method = c_char_to_string(method)?;
        let params = c_char_to_string(params)?;
        let params = serde_json::from_str(&params)?;
        let ret: String = serde_json::to_string(&executor::block_on(
            provider.request_internal(method, params, None),
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

/// Craft a new Provider with signer and callback ptr
/// # Safety
///
/// * This function cast CStr into Str
#[no_mangle]
pub unsafe extern "C" fn new_provider_with_callback(
    ice_server: *const c_char,
    stabilize_timeout: u32,
    account: *const c_char,
    account_type: *const c_char,
    signer: extern "C" fn(*const c_char, *mut c_char) -> (),
    callback_ptr: *const FFIBackendBehaviour,
) -> ProviderPtr {
    fn wrapped_signer(
        signer: extern "C" fn(*const c_char, *mut c_char) -> (),
    ) -> impl Fn(String) -> Vec<u8> {
        move |data: String| -> Vec<u8> {
            let c_data = CString::new(data).expect("Failed to convert String to CString");
            let mut sig = Vec::<u8>::with_capacity(65);
            let sig_ptr = sig.as_mut_ptr() as *mut c_char;
            signer(c_data.as_ptr(), sig_ptr);

            let c_ret = c_char_to_bytes(sig_ptr, 65).expect("Failed to convert c char to [u8]");
            let c_ret_len = c_ret.len();
            assert!(
                c_ret.len() == 65,
                "sig length({c_ret_len} < 64) is invalid: {c_ret:?}"
            );
            c_ret
        }
    }

    let provider: Provider = match (|| -> Result<Provider> {
        let ice: String = c_char_to_string(ice_server)?;
        let acc: String = c_char_to_string(account)?;
        let acc_ty: String = c_char_to_string(account_type)?;

        executor::block_on(Provider::new_provider_internal(
            ice,
            stabilize_timeout as usize,
            acc,
            acc_ty,
            Signer::Sync(Box::new(wrapped_signer(signer))),
        ))
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed on create new provider {:#}", e)
        }
    };
    let callback: &FFIBackendBehaviour = unsafe { &*callback_ptr };
    let backend = Backend::new(Arc::new(provider.clone()), Box::new(callback.clone()));
    provider
        .set_swarm_callback(Arc::new(backend))
        .expect("Failed to set callback");

    let ret: ProviderPtr = (&provider).into();
    // When leaving the closure, the origin Provider ref will be release,
    // So we manually increase the count here
    unsafe {
        ret.increase_strong_count();
    }
    ret
}

fn c_char_to_string(ptr: *const c_char) -> Result<String> {
    let c_str: &CStr = unsafe { CStr::from_ptr(ptr) };
    // Drop none utf8 sym here.
    String::from_utf8(c_str.to_owned().into()).map_err(Error::FFIFromUtf8Error)
}

fn c_char_to_bytes(ptr: *const c_char, len: usize) -> Result<Vec<u8>> {
    // Check point here.
    if ptr.is_null() {
        return Err(Error::FFINulPtrError);
    }
    let c_bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    Ok(c_bytes.to_vec())
}
