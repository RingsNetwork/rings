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
//! Please check python example at examples/ffi/rings.py

use std::collections::HashMap;
use std::error::Error as StdError;
use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use async_trait::async_trait;
use futures::executor;
use rings_core::ecc::PublicKey;
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

static FFI_E2E_INBOXES: OnceLock<Mutex<HashMap<usize, Arc<FfiE2eInbox>>>> = OnceLock::new();

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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl SwarmCallback for FfiBackend {
    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), Box<dyn StdError>> {
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

fn ffi_e2e_inboxes() -> &'static Mutex<HashMap<usize, Arc<FfiE2eInbox>>> {
    FFI_E2E_INBOXES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn provider_key(provider: &Arc<Provider>) -> usize {
    Arc::as_ptr(provider) as usize
}

fn register_ffi_e2e_inbox(provider: &Arc<Provider>, inbox: Arc<FfiE2eInbox>) -> Result<()> {
    ffi_e2e_inboxes()
        .lock()
        .map_err(|_| Error::Lock)?
        .insert(provider_key(provider), inbox);
    Ok(())
}

fn take_ffi_e2e_events(provider: &Arc<Provider>) -> Result<Vec<FfiE2eEvent>> {
    let inbox = ffi_e2e_inboxes()
        .lock()
        .map_err(|_| Error::Lock)?
        .get(&provider_key(provider))
        .cloned()
        .ok_or_else(|| Error::ExtensionError("missing FFI E2E inbox".to_string()))?;
    let mut events = inbox.lock().map_err(|_| Error::Lock)?;
    Ok(std::mem::take(&mut *events))
}

/// A structure to represent the Provider in a C-compatible format.
/// This is necessary as using Arc directly in FFI can be unsafe.
#[repr(C)]
pub struct ProviderPtr {
    provider: *const Provider,
    runtime: *const Runtime,
}

/// Provider with runtime
/// cbindgen:field-names=[]
pub(crate) struct ProviderWithRuntime {
    provider: Arc<Provider>,
    runtime: Arc<Runtime>,
}

impl ProviderWithRuntime {
    /// Create a new instance of ProviderWithRuntime
    pub fn new(p: Arc<Provider>, r: Arc<Runtime>) -> Self {
        Self {
            provider: p.clone(),
            runtime: r.clone(),
        }
    }
}

impl ProviderWithRuntime {
    /// Converts a raw ProviderPtr pointer to a Rust Provider type.
    /// # Safety
    /// Unsafe due to the dereferencing of the raw pointer.
    fn from_raw(ptr: *const ProviderPtr) -> Result<ProviderWithRuntime> {
        // Check point here.
        if ptr.is_null() {
            return Err(Error::FFINulPtrError);
        }

        let provider_ptr: &ProviderPtr = unsafe { &*ptr };
        let provider: ProviderWithRuntime = provider_ptr.into();
        // Avoid release here
        provider.check_arc();
        Ok(provider)
    }

    /// Make sure there 1 at least 5 ref to keep arc onlive
    pub fn check_arc(&self) {
        let threshold = 5;

        let p_count = Arc::strong_count(&self.provider);
        let r_count = Arc::strong_count(&self.runtime);

        if p_count < threshold {
            for _ in 0..threshold - p_count {
                unsafe { self.increase_provider_count() };
            }
            tracing::debug!("Arc<Provider> will be released when out of scope, increased")
        }

        if r_count < threshold {
            for _ in 0..threshold - r_count {
                unsafe { self.increase_runtime_count() };
            }
            tracing::debug!("Arc<Runtime> will be released when out of scope, increased")
        }
    }

    unsafe fn increase_provider_count(&self) {
        tracing::debug!("Increment strong count on provider");
        let p = Arc::into_raw(self.provider.clone());
        Arc::increment_strong_count(p);
    }

    unsafe fn increase_runtime_count(&self) {
        tracing::debug!("Decrement strong count on runtime");
        let h = Arc::into_raw(self.runtime.clone());
        Arc::increment_strong_count(h);
    }
}

impl From<&ProviderPtr> for ProviderWithRuntime {
    /// Converts a reference to a ProviderPtr to a Provider type.
    /// Note that the conversion from raw pointers to Arcs does not modify the reference count.
    /// # Safety
    /// Unsafe due to the conversion from raw pointers to Arcs.
    fn from(ptr: &ProviderPtr) -> ProviderWithRuntime {
        tracing::debug!("FFI: Provider from Ptr!");
        let provider = unsafe { Arc::<Provider>::from_raw(ptr.provider) };
        let runtime = unsafe { Arc::<Runtime>::from_raw(ptr.runtime) };

        Self { provider, runtime }
    }
}

impl From<&ProviderWithRuntime> for ProviderPtr {
    /// Cast a Provider into ProviderPtr
    fn from(provider: &ProviderWithRuntime) -> ProviderPtr {
        tracing::debug!("FFI: Provider into Ptr!");
        // Clone the Arcs, which increases the ref count,
        // then turn them into raw pointers.
        let provider_ptr = Arc::into_raw(provider.provider.clone());
        let runtime_ptr = Arc::into_raw(provider.runtime.clone());

        provider.check_arc();
        ProviderPtr {
            provider: provider_ptr,
            runtime: runtime_ptr,
        }
    }
}

/// Start message listening and stabilization
/// This function will launch listener in a new thread
/// # Safety
/// Listen function accept a ProviderPtr and will unsafety cast it into Arc based Provider
#[no_mangle]
pub extern "C" fn listen(provider_ptr: *const ProviderPtr) {
    let provider: ProviderWithRuntime =
        ProviderWithRuntime::from_raw(provider_ptr).expect("Provider ptr is invalid");
    std::thread::spawn(move || {
        provider.runtime.block_on(async {
            provider.provider.processor.listen().await;
        })
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
        let provider: ProviderWithRuntime = ProviderWithRuntime::from_raw(provider_ptr)?;

        let method = c_char_to_string(method)?;
        let params = c_char_to_string(params)?;
        let params = serde_json::from_str(&params)?;

        let ret = if method == "takeE2eEvents" {
            serde_json::to_value(TakeFfiE2eEventsResponse {
                events: take_ffi_e2e_events(&provider.provider)?,
            })?
        } else {
            let handle = std::thread::spawn(move || {
                provider
                    .runtime
                    .block_on(async { provider.provider.request_internal(method, params).await })
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
            panic!("FFI: Failed on request {e:#}")
        }
    }
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
pub unsafe extern "C" fn new_provider_with_callback(
    network_id: u32,
    ice_server: *const c_char,
    stabilize_interval: u64,
    account: *const c_char,
    account_type: *const c_char,
    signer: extern "C" fn(*const c_char, *mut c_char) -> (),
) -> ProviderPtr {
    fn wrapped_signer(
        signer: extern "C" fn(*const c_char, *mut c_char) -> (),
    ) -> impl Fn(String) -> Vec<u8> {
        move |data: String| -> Vec<u8> {
            let c_data = CString::new(data).expect("Failed to convert String to CString");
            // 64 bytes sig + \0 here
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
            network_id,
            ice,
            stabilize_interval,
            acc,
            acc_ty,
            Signer::Sync(Box::new(wrapped_signer(signer))),
            None,
            None,
        ))
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed on create new provider {e:#}")
        }
    };
    let runtime = Arc::new(Runtime::new().expect("Failed to create runtime"));
    let provider = Arc::new(provider.clone());
    let backend = Backend::new(provider.clone());
    let e2e_events = Arc::new(Mutex::new(Vec::new()));
    let callback = FfiBackend::new(backend, e2e_events.clone());

    provider
        .set_swarm_callback_internal(Arc::new(callback))
        .expect("Failed to set callback");
    register_ffi_e2e_inbox(&provider, e2e_events).expect("Failed to register FFI E2E inbox");
    let ret: ProviderPtr = (&ProviderWithRuntime::new(provider.clone(), runtime.clone())).into();
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
