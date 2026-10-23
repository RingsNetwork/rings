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
//! 2. **Application inbox for FFI**: the extension [`Backend`] consumes the swarm's application
//!    inbox on the provider runtime; its observer retains E2E messages, bounded, for the
//!    `takeE2eEvents` request, which also reports how many it dropped. The
//!    `dispatchDropCounters` request reports the envelopes the backend dropped, by reason.
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
//!
//! [`Backend`]: crate::extension::Backend

use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;
use std::ptr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use rings_core::ecc::PublicKey;
use rings_core::lifecycle::StopSource;
use rings_core::swarm::application::ApplicationData;
use rings_core::swarm::application::ApplicationMessage;
use serde::Serialize;
use tokio::runtime::Runtime;

use super::Provider;
use super::Signer;
use crate::error::Error;
use crate::error::Result;
use crate::extension::BackendObserver;
use crate::native::stores::NodeStores;

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

/// The answer to `takeE2eEvents`.
#[derive(Serialize)]
struct TakeFfiE2eEventsResponse {
    /// The events retained since the previous take.
    events: Vec<FfiE2eEvent>,
    /// The events dropped because the inbox was full, cumulative since the provider was created.
    dropped: u64,
}

/// E2E events retained for the C caller to take: bounded, so a caller that never takes them
/// costs at most [`FFI_E2E_EVENT_CAPACITY`] events, and a further event is dropped, counted.
#[derive(Default)]
struct FfiE2eInbox {
    events: Mutex<Vec<FfiE2eEvent>>,
    dropped: AtomicU64,
}

/// E2E events an [`FfiE2eInbox`] retains before it drops further ones.
const FFI_E2E_EVENT_CAPACITY: usize = 4096;

impl FfiE2eInbox {
    /// Take every retained event, with the count of those dropped so far.
    fn take(&self) -> Result<TakeFfiE2eEventsResponse> {
        let mut events = self.events.lock().map_err(|_| Error::Lock)?;
        Ok(TakeFfiE2eEventsResponse {
            events: std::mem::take(&mut *events),
            dropped: self.dropped.load(Ordering::Relaxed),
        })
    }

    /// Retain `event`, or drop it, counted, when full.
    fn push(&self, event: FfiE2eEvent) -> Result<()> {
        let mut events = self.events.lock().map_err(|_| Error::Lock)?;
        if events.len() < FFI_E2E_EVENT_CAPACITY {
            events.push(event);
        } else {
            let dropped = self
                .dropped
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            tracing::warn!(dropped, "FFI E2E event inbox full; event dropped");
        }
        Ok(())
    }
}

impl BackendObserver for FfiE2eInbox {
    /// Retain an E2E message as an FFI event.
    fn e2e_message(&self, message: &ApplicationMessage) {
        match ffi_e2e_event(message).and_then(|event| event.map_or(Ok(()), |e| self.push(e))) {
            Ok(()) => {}
            Err(error) => tracing::warn!(error = ?error, "FFI E2E event not retained"),
        }
    }
}

/// The FFI event of an E2E `message`; `None` for any other application data.
fn ffi_e2e_event(message: &ApplicationMessage) -> Result<Option<FfiE2eEvent>> {
    let from = message.from.to_string();
    Ok(Some(match &message.data {
        ApplicationData::E2eHandshakeRequest(request) => FfiE2eEvent {
            kind: "handshakeRequest",
            from,
            public_key: Some(public_key_json_string(request.requester_public_key)?),
            stream_id: None,
            sequence: None,
            is_final: None,
            ciphertext_blocks: None,
        },
        ApplicationData::E2eHandshakeResponse(response) => FfiE2eEvent {
            kind: "handshakeResponse",
            from,
            public_key: Some(public_key_json_string(response.responder_public_key)?),
            stream_id: None,
            sequence: None,
            is_final: None,
            ciphertext_blocks: None,
        },
        ApplicationData::E2eStreamFrame(frame) => FfiE2eEvent {
            kind: "streamFrame",
            from,
            public_key: Some(public_key_json_string(frame.sender_public_key)?),
            stream_id: Some(frame.stream_id.to_string()),
            sequence: Some(frame.sequence),
            is_final: Some(frame.is_final),
            ciphertext_blocks: Some(frame.ciphertext.len()),
        },
        _ => return Ok(None),
    }))
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
                if let Err(error) = provider.listen_with(stop).await {
                    tracing::error!("FFI listen failed: {error}");
                }
            })
        }));
        Ok(())
    }

    /// Serve the swarm's application inbox with the extension [`Backend`], whose observer
    /// retains E2E events for the caller, on the provider runtime until the handle stops.
    ///
    /// [`Backend`]: crate::extension::Backend
    fn spawn_application(&self) -> Result<()> {
        let service = self
            .provider
            .application_service(Some(self.e2e_events.clone()), self.stop.token())?;
        self.runtime.spawn(service);
        Ok(())
    }

    fn take_e2e_events(&self) -> Result<TakeFfiE2eEventsResponse> {
        self.e2e_events.take()
    }

    fn shutdown(self) {
        let Self {
            provider,
            runtime,
            e2e_events: _,
            stop,
            listener_threads,
        } = self;
        stop.request_stop();
        let listener_threads = match listener_threads.into_inner() {
            Ok(listener_threads) => listener_threads,
            Err(poisoned) => poisoned.into_inner(),
        };
        for listener_thread in listener_threads {
            if listener_thread.join().is_err() {
                tracing::warn!("FFI provider listener thread panicked during shutdown");
            }
        }
        flush_provider_before_runtime_shutdown(&provider, &runtime);
        drop(provider);
        // Invariant: joined listeners, the completed flush thread, and the dropped provider have
        // released every intentional runtime clone. If an unexpected in-flight owner remains,
        // leaking the final Arc avoids dropping a Tokio runtime from an asynchronous/C-ABI path.
        match Arc::try_unwrap(runtime) {
            Ok(runtime) => runtime.shutdown_background(),
            Err(runtime) => {
                tracing::error!(
                    "FFI runtime still has owners after listener shutdown; leaking it to avoid a panic across the C ABI"
                );
                std::mem::forget(runtime);
            }
        }
    }
}

fn flush_provider_before_runtime_shutdown(provider: &Arc<Provider>, runtime: &Arc<Runtime>) {
    let provider = provider.clone();
    let runtime = runtime.clone();
    let flush_thread = std::thread::Builder::new()
        .name("rings-ffi-measurement-flush".to_owned())
        .spawn(move || runtime.block_on(provider.flush_measurements()));
    match flush_thread {
        Ok(flush_thread) => match flush_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "failed to flush FFI provider measurements during shutdown");
            }
            Err(_) => {
                tracing::warn!("FFI measurement flush thread panicked during shutdown");
            }
        },
        Err(error) => {
            tracing::warn!(%error, "failed to start FFI measurement flush thread during shutdown");
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
            serde_json::to_value(handle.take_e2e_events()?)?
        } else if method == "dispatchDropCounters" {
            serde_json::to_value(handle.provider.dispatch_drop_counters())?
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
/// shutdown, joins listener threads started through [`rings_node_listen`],
/// explicitly flushes measurement state even when listening never started, and
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

/// Craft a new Provider with signer, keeping its stores under `storage_path`.
///
/// `storage_path` is a directory the provider owns: it opens the native node's stores under it,
/// each at its authority, as the daemon does ([`NodeStores::open_under`]): the DHT data store in
/// `data`, the transaction replay store in `transaction-replay` (authoritative), the relay inbox
/// in `relay-inbox` (retained), the onion entry guards in `onion-entry-guards` (a durable cache),
/// and the measurement stores. Reusing the same path across runs keeps the replay windows, held
/// mail and entry guards (#1022); a fresh path is a fresh node state. Creation fails, returning
/// NULL, when a store cannot be opened.
///
/// Precondition: the provider owns `storage_path` exclusively while it lives. At most one live
/// provider uses a given root, and it is never a native daemon's storage directory: every store
/// assumes it is the only writer of its directory, and two writers would interleave their writes
/// to one authoritative replay store. Nothing enforces this yet (#1039).
///
/// Installs the extension [`Backend`] so inbound custom messages are decoded as
/// namespaced envelopes and routed to the protocol registry. (The old per-variant C
/// message callback is gone with `BackendMessage`; an FFI protocol-registration path
/// would replace it.)
///
/// [`Backend`]: crate::extension::Backend
///
/// # Safety
///
/// String pointers, `storage_path` included, must be valid null-terminated UTF-8 strings. The
/// signer must write exactly 65 signature bytes into the provided output buffer. Returns NULL
/// when provider creation fails. The returned handle must be released exactly once with
/// `rings_node_provider_destroy`.
#[no_mangle]
pub unsafe extern "C" fn rings_node_new_provider_with_callback(
    network_id: u32,
    ice_server: *const c_char,
    stabilize_interval: u64,
    account: *const c_char,
    account_type: *const c_char,
    storage_path: *const c_char,
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
        let storage_root = std::path::PathBuf::from(c_char_to_string(storage_path)?);

        let creation = std::thread::Builder::new()
            .name("rings-ffi-provider-init".to_owned())
            .spawn(move || -> Result<(Provider, Arc<Runtime>)> {
                let runtime = Arc::new(Runtime::new().map_err(|error| {
                    Error::ExtensionError(format!("failed to create runtime: {error}"))
                })?);
                let provider = runtime.block_on(async {
                    let stores = NodeStores::open_under(&storage_root).await?;
                    Provider::new_provider_internal(
                        network_id,
                        ice,
                        stabilize_interval,
                        acc,
                        acc_ty,
                        Signer::Sync(Box::new(wrapped_signer(signer))),
                        Some(stores.data),
                        Some(stores.relay),
                        Some(stores.measure),
                        Some(stores.evidence),
                        Some(stores.guards),
                        Some(stores.replay),
                    )
                    .await
                })?;
                Ok((provider, runtime))
            })
            .map_err(|error| {
                Error::ExtensionError(format!("failed to start FFI provider runtime: {error}"))
            })?;
        let (provider, runtime) = creation.join().map_err(|_| {
            Error::ExtensionError("FFI provider initialization thread panicked".to_owned())
        })??;
        let provider = Arc::new(provider);
        let e2e_events = Arc::new(FfiE2eInbox::default());
        let handle = ProviderHandle::new(provider, runtime, e2e_events);
        handle.spawn_application()?;
        Ok(Box::into_raw(Box::new(handle)))
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

    use async_trait::async_trait;
    use rings_core::delegation::DelegateeKey;
    use rings_core::dht::Did;
    use rings_core::ecc::signers::eip191;
    use rings_core::ecc::SecretKey;
    use rings_core::measure::MeasureError;
    use rings_core::storage::KvStorageInterface;
    use rings_measure::MeasurementSnapshot;

    use super::*;
    use crate::processor::ProcessorConfig;

    const TEST_ACCOUNT: &str = "0x11E807fcc88dD319270493fB2e822e388Fe36ab0";
    const TEST_SECRET_KEY: &str =
        "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0";

    #[derive(Clone, Default)]
    struct ObservedMeasureStorage {
        value: Arc<Mutex<Option<MeasurementSnapshot<Did>>>>,
    }

    fn poisoned_measure_storage() -> rings_core::error::Error {
        std::io::Error::other("observed measurement storage mutex poisoned").into()
    }

    #[async_trait]
    impl KvStorageInterface<MeasurementSnapshot<Did>> for ObservedMeasureStorage {
        async fn get(
            &self,
            _key: &str,
        ) -> rings_core::error::Result<Option<MeasurementSnapshot<Did>>> {
            self.value
                .lock()
                .map(|value| value.clone())
                .map_err(|_| poisoned_measure_storage())
        }

        async fn put(
            &self,
            _key: &str,
            value: &MeasurementSnapshot<Did>,
        ) -> rings_core::error::Result<()> {
            *self.value.lock().map_err(|_| poisoned_measure_storage())? = Some(value.clone());
            Ok(())
        }

        async fn get_all(
            &self,
        ) -> rings_core::error::Result<Vec<(String, MeasurementSnapshot<Did>)>> {
            Ok(Vec::new())
        }

        async fn remove(&self, _key: &str) -> rings_core::error::Result<()> {
            *self.value.lock().map_err(|_| poisoned_measure_storage())? = None;
            Ok(())
        }

        async fn clear(&self) -> rings_core::error::Result<()> {
            *self.value.lock().map_err(|_| poisoned_measure_storage())? = None;
            Ok(())
        }

        async fn count(&self) -> rings_core::error::Result<u32> {
            self.value
                .lock()
                .map(|value| u32::from(value.is_some()))
                .map_err(|_| poisoned_measure_storage())
        }
    }

    extern "C" fn test_signer(data: *const c_char, output: *mut c_char) {
        if data.is_null() || output.is_null() {
            return;
        }
        let data = unsafe { CStr::from_ptr(data) };
        let key = SecretKey::try_from(TEST_SECRET_KEY).expect("valid test key");
        let signature = eip191::sign_raw(&key, data.to_bytes()).expect("valid test signature");
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

    /// A fresh storage root under the system temporary directory, as a path and a C string.
    fn storage_root() -> (std::path::PathBuf, CString) {
        let root = std::env::temp_dir().join(format!("rings-ffi-{}", uuid::Uuid::new_v4()));
        let c_root = c_string(&root.to_string_lossy());
        (root, c_root)
    }

    /// Create, use and destroy a provider three times over one storage root: each run reopens
    /// the stores the previous one left there (#1022).
    #[test]
    fn test_ffi_provider_handle_create_listen_request_destroy_cycles() {
        let (root, storage_path) = storage_root();
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
                    storage_path.as_ptr(),
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
            assert!(events_response.contains("\"dropped\":0"));
            let drops_response = request(handle, "dispatchDropCounters", "{}");
            assert!(drops_response.contains("\"queueFull\":0"));

            let weak_provider = unsafe { Arc::downgrade(&(*handle).provider) };
            unsafe { rings_node_provider_destroy(handle) };
            assert!(
                weak_provider.upgrade().is_none(),
                "destroy must release the provider callback reference cycle"
            );
        }
        assert!(root.join("transaction-replay").is_dir());
        assert!(root.join("relay-inbox").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_ffi_free_functions_accept_null() {
        unsafe {
            rings_node_string_free(ptr::null_mut());
            rings_node_provider_destroy(ptr::null_mut());
        }
    }

    #[tokio::test]
    async fn test_ffi_provider_creation_and_destroy_are_safe_inside_tokio() {
        let ice_server = c_string("stun://stun.l.google.com");
        let account = c_string(TEST_ACCOUNT);
        let account_type = c_string("eip191");
        let (root, storage_path) = storage_root();
        let handle = unsafe {
            rings_node_new_provider_with_callback(
                0,
                ice_server.as_ptr(),
                1,
                account.as_ptr(),
                account_type.as_ptr(),
                storage_path.as_ptr(),
                test_signer,
            )
        };

        assert!(!handle.is_null());
        unsafe { rings_node_provider_destroy(handle) };
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_ffi_destroy_without_listen_flushes_dirty_measurements() {
        let runtime = Arc::new(Runtime::new().expect("test runtime must initialize"));
        let storage = ObservedMeasureStorage::default();
        let provider = runtime
            .block_on(async {
                let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
                let config = ProcessorConfig::new(
                    0,
                    "stun://stun.l.google.com:19302".to_owned(),
                    delegatee_key,
                    1,
                );
                Provider::new_provider_with_storage_internal(
                    config,
                    None,
                    None,
                    Some(Box::new(storage.clone())),
                    None,
                    None,
                    None,
                )
                .await
            })
            .expect("test provider must initialize");
        let provider = Arc::new(provider);
        let peer: Did = SecretKey::random().address().into();
        runtime
            .block_on(
                provider
                    .processor
                    .record_authenticated_measurement_for_test(peer),
            )
            .expect("test mutation must be applied");
        ProviderHandle::new(provider, runtime, Arc::default()).shutdown();

        let snapshot = storage
            .value
            .lock()
            .expect("observed storage mutex must remain available")
            .clone()
            .expect("destroy must flush the dirty measurement snapshot");
        let ledger = rings_measure::MeasurementLedger::from_snapshot(snapshot).unwrap_or_else(
            |error: MeasureError| panic!("flushed snapshot must validate: {error}"),
        );
        let record = ledger
            .record(&peer)
            .expect("authenticated test measurement must be retained");
        assert_eq!(record.reliability().stored_evidence().connected, 1);
    }
}
