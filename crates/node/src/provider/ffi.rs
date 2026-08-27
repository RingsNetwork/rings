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
//! 1. **Provider Representation for FFI**: The module defines `ProviderHandle`, an opaque,
//!    destructible handle that owns the Rust provider resources behind a raw C pointer. Callers
//!    never receive raw `Arc` ownership tokens.
//!
//! 2. **Message Callback for FFI**: The `SwarmCallbackInstanceFFI` struct serves as a bridge
//!    for message callback functionalities between Rust and other languages. It can hold
//!    function pointers to C-compatible functions that handle custom and built-in messages.
//!
//! 3. **Functions for Provider Interaction**: The `rings_node_*` C ABI functions create providers,
//!    start listeners, configure logging, and make internal requests. The prefix prevents symbol
//!    collisions with platform functions such as POSIX `listen(2)`.
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
//! Please check python example at examples/ffi/rings.py

use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::executor;
use rings_core::ecc::PublicKey;
use rings_core::lifecycle::StopSource;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use rings_core::swarm::callback::SwarmCallback;
use serde::Serialize;
use tokio::runtime::Runtime;

use super::Provider;
use super::Signer;
use crate::error::Error;
use crate::error::Result;
use crate::extension::Backend;

type FfiE2eInbox = Mutex<Vec<FfiE2eEvent>>;

const FFI_SIGNATURE_LEN: usize = 65;

#[derive(Clone, Debug, Serialize)]
struct FfiE2eEvent {
    kind: &'static str,
    from: String,
    public_key: Option<String>,
    stream_id: Option<String>,
    sequence: Option<u64>,
    is_final: Option<bool>,
    ciphertext_blocks: Option<usize>,
}

#[derive(Serialize)]
struct TakeFfiE2eEventsResponse {
    events: Vec<FfiE2eEvent>,
}

struct FfiBackend {
    backend: Backend,
    e2e_events: Arc<FfiE2eInbox>,
}

impl FfiBackend {
    fn new(backend: Backend, e2e_events: Arc<FfiE2eInbox>) -> Self {
        Self {
            backend,
            e2e_events,
        }
    }

    fn push_e2e_event(&self, event: FfiE2eEvent) -> Result<()> {
        self.e2e_events.lock().map_err(|_| Error::Lock)?.push(event);
        Ok(())
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl SwarmCallback for FfiBackend {
    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), rings_core::error::CallbackError> {
        let data: Message = payload.transaction.data()?;
        let from = payload.transaction.signer().to_string();

        match data {
            Message::CustomMessage(_) => self.backend.on_inbound(payload).await?,
            Message::E2eHandshakeRequest(request) => self.push_e2e_event(FfiE2eEvent {
                kind: "handshakeRequest",
                from,
                public_key: Some(public_key_json_string(request.requester_public_key)?),
                stream_id: None,
                sequence: None,
                is_final: None,
                ciphertext_blocks: None,
            })?,
            Message::E2eHandshakeResponse(response) => self.push_e2e_event(FfiE2eEvent {
                kind: "handshakeResponse",
                from,
                public_key: Some(public_key_json_string(response.responder_public_key)?),
                stream_id: None,
                sequence: None,
                is_final: None,
                ciphertext_blocks: None,
            })?,
            Message::E2eStreamFrame(frame) => self.push_e2e_event(FfiE2eEvent {
                kind: "streamFrame",
                from,
                public_key: Some(public_key_json_string(frame.sender_public_key)?),
                stream_id: Some(frame.stream_id.to_string()),
                sequence: Some(frame.sequence),
                is_final: Some(frame.is_final),
                ciphertext_blocks: Some(frame.ciphertext.len()),
            })?,
            _ => {}
        }

        Ok(())
    }
}

fn public_key_json_string(public_key: PublicKey<33>) -> Result<String> {
    let value = serde_json::to_value(public_key)?;
    value.as_str().map(str::to_owned).ok_or(Error::InvalidData)
}

/// Opaque provider handle owned by the C ABI caller.
///
/// The handle owns ordinary Rust [`Arc`] values and is released exactly once by
/// [`rings_node_provider_destroy`]. C callers must treat `ProviderHandle` as an
/// incomplete type and must never inspect or copy its contents.
pub struct ProviderHandle {
    provider: Arc<Provider>,
    runtime: Arc<Runtime>,
    e2e_events: Arc<FfiE2eInbox>,
    stop: StopSource,
    listener_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl ProviderHandle {
    fn new(provider: Arc<Provider>, runtime: Arc<Runtime>, e2e_events: Arc<FfiE2eInbox>) -> Self {
        Self {
            provider,
            runtime,
            e2e_events,
            stop: StopSource::new(),
            listener_threads: Mutex::new(Vec::new()),
        }
    }

    unsafe fn from_ptr<'a>(ptr: *const ProviderHandle) -> Result<&'a ProviderHandle> {
        if ptr.is_null() {
            return Err(Error::FFINulPtrError);
        }

        Ok(unsafe { &*ptr })
    }

    fn spawn_listener(&self) -> Result<()> {
        let provider = self.provider.clone();
        let runtime = self.runtime.clone();
        let stop = self.stop.token();
        let mut listener_threads = self.listener_threads.lock().map_err(|_| Error::Lock)?;
        listener_threads.push(std::thread::spawn(move || {
            runtime.block_on(async {
                provider.listen_with(stop).await;
            })
        }));
        Ok(())
    }

    fn take_e2e_events(&self) -> Result<Vec<FfiE2eEvent>> {
        let mut events = self.e2e_events.lock().map_err(|_| Error::Lock)?;
        Ok(std::mem::take(&mut *events))
    }

    fn shutdown(self) {
        self.stop.request_stop();
        let listener_threads = match self.listener_threads.into_inner() {
            Ok(listener_threads) => listener_threads,
            Err(poisoned) => poisoned.into_inner(),
        };
        for listener_thread in listener_threads {
            if listener_thread.join().is_err() {
                tracing::warn!("FFI provider listener thread panicked during shutdown");
            }
        }
        if let Err(error) = self.provider.clear_swarm_callback_internal() {
            tracing::warn!("failed to clear FFI provider callback during shutdown: {error}");
        }
    }
}

/// Start message listening and stabilization.
///
/// This function launches a cooperative listener thread owned by the provider
/// handle. The listener is stopped and joined by [`rings_node_provider_destroy`].
///
/// # Safety
///
/// `provider_ptr` must be null or a live handle returned by
/// [`rings_node_new_provider_with_callback`]. A destroyed handle must not be
/// used again.
#[no_mangle]
pub unsafe extern "C" fn rings_node_listen(provider_ptr: *const ProviderHandle) {
    let provider = match unsafe { ProviderHandle::from_ptr(provider_ptr) } {
        Ok(provider) => provider,
        Err(error) => {
            tracing::error!("FFI listen failed: {error}");
            return;
        }
    };
    if let Err(error) = provider.spawn_listener() {
        tracing::error!("FFI listen failed: {error}");
    }
}

/// Request internal rpc api
///
/// Returns a newly allocated UTF-8 JSON string on success. Call
/// [`rings_node_string_free`] exactly once for every non-null returned pointer.
///
/// # Safety
///
/// * `provider_ptr` must be null or a live handle returned by
///   [`rings_node_new_provider_with_callback`]. A destroyed handle must not be
///   used again.
/// * `method` and `params` must be valid null-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn rings_node_request(
    provider_ptr: *const ProviderHandle,
    method: *const c_char,
    params: *const c_char,
) -> *mut c_char {
    match (|| -> Result<*mut c_char> {
        let handle = unsafe { ProviderHandle::from_ptr(provider_ptr) }?;

        let method = c_char_to_string(method)?;
        let params = c_char_to_string(params)?;
        let params = serde_json::from_str(&params)?;

        let ret = if method == "takeE2eEvents" {
            serde_json::to_value(TakeFfiE2eEventsResponse {
                events: handle.take_e2e_events()?,
            })?
        } else {
            let provider = handle.provider.clone();
            let runtime = handle.runtime.clone();
            let handle = std::thread::spawn(move || {
                runtime.block_on(async { provider.request_internal(method, params).await })
            });
            handle
                .join()
                .map_err(|_| Error::ExtensionError("FFI request thread panicked".to_string()))??
        };
        let ret: String = serde_json::to_string(&ret)?;
        let c_ret = CString::new(ret)?.into_raw();
        Ok(c_ret)
    })() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("FFI Request failed, cause by: {:?}", e);
            ptr::null_mut()
        }
    }
}

/// Free a string returned by [`rings_node_request`].
///
/// Passing null is a no-op. Passing any pointer not returned by
/// [`rings_node_request`], or freeing the same pointer twice, is undefined
/// behavior.
///
/// # Safety
///
/// `value` must be null or a pointer returned by [`rings_node_request`] that has
/// not already been freed.
#[no_mangle]
pub unsafe extern "C" fn rings_node_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(value) });
}

/// Destroy a provider handle returned by [`rings_node_new_provider_with_callback`].
///
/// Passing null is a no-op. The function requests cooperative listener
/// shutdown, joins listener threads started through [`rings_node_listen`], and
/// then releases the handle. The pointer is invalid after this call.
///
/// # Safety
///
/// `provider_ptr` must be null or a live handle returned by
/// [`rings_node_new_provider_with_callback`]. It must be destroyed exactly once
/// and must not be used concurrently by other FFI calls while destruction runs.
#[no_mangle]
pub unsafe extern "C" fn rings_node_provider_destroy(provider_ptr: *mut ProviderHandle) {
    if provider_ptr.is_null() {
        return;
    }
    let provider = unsafe { Box::from_raw(provider_ptr) };
    provider.shutdown();
}

/// Craft a new Provider with signer.
///
/// Installs the extension [`Backend`] so inbound custom messages are decoded as
/// namespaced envelopes and routed to the protocol registry. (The old per-variant C
/// message callback is gone with `BackendMessage`; an FFI protocol-registration path
/// would replace it.)
///
/// # Safety
///
/// * This function cast CStr into Str
#[no_mangle]
pub unsafe extern "C" fn rings_node_new_provider_with_callback(
    network_id: u32,
    ice_server: *const c_char,
    stabilize_interval: u64,
    account: *const c_char,
    account_type: *const c_char,
    signer: extern "C" fn(*const c_char, *mut c_char) -> (),
) -> *mut ProviderHandle {
    fn wrapped_signer(
        signer: extern "C" fn(*const c_char, *mut c_char) -> (),
    ) -> impl Fn(String) -> Vec<u8> {
        move |data: String| -> Vec<u8> {
            let c_data = match CString::new(data) {
                Ok(value) => value,
                Err(error) => {
                    tracing::error!("FFI signer input contains nul byte: {error}");
                    return Vec::new();
                }
            };
            let mut sig = [0_u8; FFI_SIGNATURE_LEN];
            let sig_ptr = sig.as_mut_ptr().cast::<c_char>();
            signer(c_data.as_ptr(), sig_ptr);
            sig.to_vec()
        }
    }

    match (|| -> Result<*mut ProviderHandle> {
        let ice: String = c_char_to_string(ice_server)?;
        let acc: String = c_char_to_string(account)?;
        let acc_ty: String = c_char_to_string(account_type)?;

        let provider = executor::block_on(Provider::new_provider_internal(
            network_id,
            ice,
            stabilize_interval,
            acc,
            acc_ty,
            Signer::Sync(Box::new(wrapped_signer(signer))),
            None,
            None,
        ))?;
        let runtime = Arc::new(Runtime::new().map_err(|error| {
            Error::ExtensionError(format!("failed to create runtime: {error}"))
        })?);
        let provider = Arc::new(provider);
        let backend = Backend::new(provider.clone());
        let e2e_events = Arc::new(Mutex::new(Vec::new()));
        let callback = FfiBackend::new(backend, e2e_events.clone());

        provider.set_swarm_callback_internal(Arc::new(callback))?;
        Ok(Box::into_raw(Box::new(ProviderHandle::new(
            provider, runtime, e2e_events,
        ))))
    })() {
        Ok(provider_ptr) => provider_ptr,
        Err(error) => {
            tracing::error!("FFI provider creation failed: {error}");
            ptr::null_mut()
        }
    }
}

fn c_char_to_string(ptr: *const c_char) -> Result<String> {
    if ptr.is_null() {
        return Err(Error::FFINulPtrError);
    }
    let c_str: &CStr = unsafe { CStr::from_ptr(ptr) };
    // Drop none utf8 sym here.
    String::from_utf8(c_str.to_owned().into()).map_err(Error::FFIFromUtf8Error)
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::ffi::CString;

    use rings_core::ecc::signers::eip191;
    use rings_core::ecc::SecretKey;

    use super::*;

    const TEST_ACCOUNT: &str = "0x11E807fcc88dD319270493fB2e822e388Fe36ab0";
    const TEST_SECRET_KEY: &str =
        "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0";

    extern "C" fn test_signer(data: *const c_char, output: *mut c_char) {
        if data.is_null() || output.is_null() {
            return;
        }
        let data = unsafe { CStr::from_ptr(data) };
        let key = SecretKey::try_from(TEST_SECRET_KEY).expect("valid test key");
        let signature = eip191::sign_raw(key, data.to_bytes());
        unsafe {
            std::ptr::copy_nonoverlapping(
                signature.as_ptr().cast::<c_char>(),
                output,
                FFI_SIGNATURE_LEN,
            );
        }
    }

    fn c_string(value: &str) -> CString {
        CString::new(value).expect("test string has no nul bytes")
    }

    fn request(handle: *const ProviderHandle, method: &str, params: &str) -> String {
        let method = c_string(method);
        let params = c_string(params);
        let response = unsafe { rings_node_request(handle, method.as_ptr(), params.as_ptr()) };
        assert!(!response.is_null());
        let response_string = unsafe { CStr::from_ptr(response) }
            .to_str()
            .expect("response is utf-8")
            .to_owned();
        unsafe { rings_node_string_free(response) };
        response_string
    }

    #[test]
    fn test_ffi_provider_handle_create_listen_request_destroy_cycles() {
        for _ in 0..3 {
            let ice_server = c_string("stun://stun.l.google.com");
            let account = c_string(TEST_ACCOUNT);
            let account_type = c_string("eip191");
            let handle = unsafe {
                rings_node_new_provider_with_callback(
                    0,
                    ice_server.as_ptr(),
                    1,
                    account.as_ptr(),
                    account_type.as_ptr(),
                    test_signer,
                )
            };
            assert!(!handle.is_null());

            unsafe { rings_node_listen(handle) };
            let did_response = request(handle, "nodeDid", "{}");
            let did_response: serde_json::Value =
                serde_json::from_str(&did_response).expect("nodeDid response is json");
            let did = did_response
                .get("did")
                .and_then(serde_json::Value::as_str)
                .expect("nodeDid response contains did");
            assert!(did.eq_ignore_ascii_case(TEST_ACCOUNT));
            let events_response = request(handle, "takeE2eEvents", "{}");
            assert!(events_response.contains("\"events\":[]"));

            let weak_provider = unsafe { Arc::downgrade(&(*handle).provider) };
            unsafe { rings_node_provider_destroy(handle) };
            assert!(
                weak_provider.upgrade().is_none(),
                "destroy must release the provider callback reference cycle"
            );
        }
    }

    #[test]
    fn test_ffi_free_functions_accept_null() {
        unsafe {
            rings_node_string_free(ptr::null_mut());
            rings_node_provider_destroy(ptr::null_mut());
        }
    }
}
