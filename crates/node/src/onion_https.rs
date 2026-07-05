#![warn(missing_docs)]
//! Browser HTTPS onion-exit request/response adapter.
//!
//! This protocol is intentionally application-layer HTTPS. Browser exits cannot expose raw TCP,
//! so a client sends an HTTPS request description over the route-aware onion circuit, the exit
//! performs `fetch`, and the response is sent back over the circuit return path.
//!
//! A browser page exit is constrained by the host browser's `fetch` capability: CORS, forbidden
//! headers, credentials policy, and extension host permissions still apply. A full arbitrary HTTPS
//! exit must run in a browser-extension or native context that grants those fetch permissions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::oneshot;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::send_backward;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionHttpsRequest;
use crate::onion::circuit::OnionHttpsResponse;
use crate::onion::OnionExitPolicy;
use crate::onion_proxy::OnionProxyTarget;

const EXIT_LIMIT_WINDOW_MS: u128 = 60_000;

lazy_static::lazy_static! {
    static ref RUNTIMES: Mutex<HashMap<String, Arc<OnionHttpsRuntime>>> =
        Mutex::new(HashMap::new());
}

/// Return the shared HTTPS proxy runtime for one local provider instance.
pub(crate) fn runtime_for_provider(provider_key: String) -> Result<Arc<OnionHttpsRuntime>> {
    let mut runtimes = RUNTIMES.lock().map_err(|_| Error::Lock)?;
    Ok(runtimes
        .entry(provider_key)
        .or_insert_with(|| Arc::new(OnionHttpsRuntime::new()))
        .clone())
}

/// JS-facing request fields for one HTTPS proxy request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct OnionHttpsClientRequest {
    /// HTTP method. Defaults to `GET`.
    #[serde(default = "default_method")]
    pub method: String,
    /// Optional path and query override. Defaults to the request URL path, then `/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Request headers.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Request body bytes.
    #[serde(default)]
    pub body: Vec<u8>,
}

impl Default for OnionHttpsClientRequest {
    fn default() -> Self {
        Self {
            method: default_method(),
            path: None,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// JS-facing response fields returned from one HTTPS proxy request.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct OnionHttpsClientResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Shared runtime for the local browser HTTPS proxy protocol.
#[derive(Default)]
pub(crate) struct OnionHttpsRuntime {
    circuit_protocol_installed: AtomicBool,
    next_request: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    exit_policy: Mutex<Option<OnionExitPolicy>>,
    limiter: Arc<Mutex<ExitLimiter>>,
}

struct PendingRequest {
    expected_return_peer: Did,
    sender: oneshot::Sender<std::result::Result<OnionHttpsClientResponse, String>>,
}

#[derive(Default)]
struct ExitLimiter {
    active_circuits: u32,
    active_streams_by_circuit: HashMap<ExitCircuitKey, u32>,
    window_start_ms: u128,
    bytes_this_window: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExitCircuitKey {
    return_path: Vec<Did>,
}

impl ExitCircuitKey {
    fn new(return_path: &[Did]) -> Self {
        Self {
            return_path: return_path.to_vec(),
        }
    }
}

struct ExitCircuitLease {
    limiter: Arc<Mutex<ExitLimiter>>,
    circuit: ExitCircuitKey,
}

impl Drop for ExitCircuitLease {
    fn drop(&mut self) {
        if let Ok(mut limiter) = self.limiter.lock() {
            if let Some(active_streams) = limiter.active_streams_by_circuit.get_mut(&self.circuit) {
                if *active_streams > 1 {
                    *active_streams -= 1;
                } else {
                    limiter.active_streams_by_circuit.remove(&self.circuit);
                    limiter.active_circuits = limiter.active_circuits.saturating_sub(1);
                }
            }
        }
    }
}

impl OnionHttpsRuntime {
    /// Create an empty runtime.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return whether this runtime installed the shared circuit protocol.
    pub(crate) fn circuit_protocol_installed(&self) -> bool {
        self.circuit_protocol_installed.load(Ordering::Acquire)
    }

    /// Mark the shared circuit protocol as installed by this runtime.
    pub(crate) fn mark_circuit_protocol_installed(&self) {
        self.circuit_protocol_installed
            .store(true, Ordering::Release);
    }

    /// Set the local exit policy. `None` means client-only mode.
    pub(crate) fn set_exit_policy(&self, policy: Option<OnionExitPolicy>) {
        if let Ok(mut current) = self.exit_policy.lock() {
            *current = policy;
        }
    }

    /// Begin a client request expected to complete from the immediate return peer.
    pub(crate) fn begin_request(
        &self,
        expected_return_peer: Did,
    ) -> Result<(
        u64,
        oneshot::Receiver<std::result::Result<OnionHttpsClientResponse, String>>,
    )> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| Error::Lock)?
            .insert(id, PendingRequest {
                expected_return_peer,
                sender,
            });
        Ok((id, receiver))
    }

    /// Cancel a request that failed before it was sent.
    pub(crate) fn cancel_request(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// Complete a pending HTTPS request with a response.
    pub(crate) fn complete_response(&self, from: Did, id: u64, response: OnionHttpsResponse) {
        let Some(pending) = self.take_pending(from, id) else {
            return;
        };
        let _ = pending.sender.send(Ok(OnionHttpsClientResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        }));
    }

    /// Complete a pending HTTPS request with an error.
    pub(crate) fn complete_error(&self, from: Did, id: u64, message: String) {
        let Some(pending) = self.take_pending(from, id) else {
            return;
        };
        let _ = pending.sender.send(Err(message));
    }

    fn take_pending(&self, from: Did, id: u64) -> Option<PendingRequest> {
        let mut pending = self.pending.lock().ok()?;
        let request = pending.remove(&id)?;
        if request.expected_return_peer == from {
            Some(request)
        } else {
            pending.insert(id, request);
            None
        }
    }

    pub(crate) fn exit_policy(&self) -> Option<OnionExitPolicy> {
        self.exit_policy
            .lock()
            .ok()
            .and_then(|policy| policy.clone())
    }

    fn admit_exit_request(
        &self,
        policy: &OnionExitPolicy,
        return_path: &[Did],
        bytes: u64,
    ) -> Result<ExitCircuitLease> {
        let circuit = ExitCircuitKey::new(return_path);
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        let active_streams = limiter
            .active_streams_by_circuit
            .get(&circuit)
            .copied()
            .unwrap_or_default();
        if policy.max_streams_per_circuit > 0 && active_streams >= policy.max_streams_per_circuit {
            return Err(Error::NoPermission);
        }
        if active_streams == 0
            && policy.max_circuits > 0
            && limiter.active_circuits >= policy.max_circuits
        {
            return Err(Error::NoPermission);
        }
        if active_streams == 0 {
            limiter.active_circuits = limiter.active_circuits.saturating_add(1);
        }
        limiter
            .active_streams_by_circuit
            .insert(circuit.clone(), active_streams.saturating_add(1));
        drop(limiter);

        let lease = ExitCircuitLease {
            limiter: self.limiter.clone(),
            circuit,
        };
        if let Err(error) = self.record_exit_bytes(policy, bytes) {
            drop(lease);
            return Err(error);
        }
        Ok(lease)
    }

    fn record_exit_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(());
        }
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        let now_ms = get_epoch_ms();
        if now_ms.saturating_sub(limiter.window_start_ms) >= EXIT_LIMIT_WINDOW_MS {
            limiter.window_start_ms = now_ms;
            limiter.bytes_this_window = 0;
        }
        let next = limiter.bytes_this_window.saturating_add(bytes);
        if next > policy.max_bytes_per_minute {
            return Err(Error::NoPermission);
        }
        limiter.bytes_this_window = next;
        Ok(())
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }
}

/// Parse a full HTTPS URL and encode one client request for its target.
pub(crate) fn client_request_from_url(
    url: &str,
    request: OnionHttpsClientRequest,
) -> Result<(OnionProxyTarget, OnionHttpsRequest)> {
    let (target, path) = parse_https_url(url)?;
    let request = client_request_with_default_path(&target, request, path.as_str())?;
    Ok((target, request))
}

fn client_request_with_default_path(
    target: &OnionProxyTarget,
    request: OnionHttpsClientRequest,
    default_path: &str,
) -> Result<OnionHttpsRequest> {
    let path = request.path.as_deref().unwrap_or(default_path);
    Ok(OnionHttpsRequest {
        target: target.authority(),
        method: normalize_method(&request.method),
        path: normalize_path(path)?,
        headers: request.headers,
        body: request.body,
    })
}

fn parse_https_url(url: &str) -> Result<(OnionProxyTarget, String)> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        Error::HttpRequestError(
            "browser HTTPS onion proxy request URL must be absolute".to_string(),
        )
    })?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(Error::HttpRequestError(format!(
            "browser HTTPS onion proxy only supports https URLs, got scheme {scheme:?}"
        )));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, suffix) = rest.split_at(authority_end);
    if authority.contains('@') {
        return Err(Error::HttpRequestError(
            "browser HTTPS onion proxy URLs must not contain userinfo".to_string(),
        ));
    }
    let authority = https_authority_with_default_port(authority)?;
    let target = OnionProxyTarget::parse_authority(authority.as_str())?;
    Ok((target, url_path(suffix)))
}

fn https_authority_with_default_port(authority: &str) -> Result<String> {
    let authority = authority.trim();
    if authority.is_empty() {
        return Err(Error::HttpRequestError(
            "browser HTTPS onion proxy URL host must not be empty".to_string(),
        ));
    }

    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return Err(Error::HttpRequestError(format!(
                "invalid IPv6 HTTPS onion proxy authority {authority:?}"
            )));
        };
        if host.is_empty() {
            return Err(Error::HttpRequestError(
                "browser HTTPS onion proxy URL host must not be empty".to_string(),
            ));
        }
        return if suffix.is_empty() {
            Ok(format!("[{host}]:443"))
        } else if let Some(port) = suffix.strip_prefix(':') {
            if port.is_empty() {
                Err(Error::HttpRequestError(format!(
                    "HTTPS onion proxy authority {authority:?} has an empty port"
                )))
            } else {
                Ok(authority.to_string())
            }
        } else {
            Err(Error::HttpRequestError(format!(
                "invalid IPv6 HTTPS onion proxy authority {authority:?}"
            )))
        };
    }

    if authority.contains('[') || authority.contains(']') {
        return Err(Error::HttpRequestError(format!(
            "invalid HTTPS onion proxy authority {authority:?}"
        )));
    }
    let colon_count = authority.chars().filter(|ch| *ch == ':').count();
    if colon_count > 1 {
        return Err(Error::HttpRequestError(
            "IPv6 HTTPS onion proxy URLs must use bracketed hosts".to_string(),
        ));
    }
    if colon_count == 1 {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return Err(Error::HttpRequestError(format!(
                "invalid HTTPS onion proxy authority {authority:?}"
            )));
        };
        if host.is_empty() || port.is_empty() {
            return Err(Error::HttpRequestError(format!(
                "invalid HTTPS onion proxy authority {authority:?}"
            )));
        }
        Ok(authority.to_string())
    } else {
        Ok(format!("{authority}:443"))
    }
}

fn url_path(suffix: &str) -> String {
    let path = suffix
        .split_once('#')
        .map_or(suffix, |(before_fragment, _)| before_fragment);
    if path.is_empty() {
        default_path()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    }
}

/// Browser handler for HTTPS onion circuits.
pub(crate) struct BrowserOnionCircuitHandler {
    https: Arc<OnionHttpsRuntime>,
}

impl BrowserOnionCircuitHandler {
    /// Create a browser circuit handler backed by the HTTPS runtime.
    pub(crate) fn new(https: Arc<OnionHttpsRuntime>) -> Self {
        Self { https }
    }
}

#[async_trait::async_trait(?Send)]
impl OnionCircuitHandler for BrowserOnionCircuitHandler {
    async fn handle_exit(
        &self,
        scope: &Scope,
        _from: Did,
        stream_id: u64,
        return_path: Vec<Did>,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        let response = match payload {
            OnionCircuitPayload::HttpsRequest(request) => {
                match execute_exit_fetch(&self.https, &request, &return_path).await {
                    Ok(response) => OnionCircuitPayload::HttpsResponse(response),
                    Err(error) => OnionCircuitPayload::HttpsError(error.to_string()),
                }
            }
            OnionCircuitPayload::TcpOpen { .. }
            | OnionCircuitPayload::TcpData { .. }
            | OnionCircuitPayload::TcpShutdown
            | OnionCircuitPayload::TcpClose
            | OnionCircuitPayload::TcpError { .. } => OnionCircuitPayload::TcpError {
                message: "browser onion exits do not support TCP".to_string(),
            },
            OnionCircuitPayload::HttpsResponse(_) | OnionCircuitPayload::HttpsError(_) => {
                return Ok(());
            }
        };
        send_backward(scope, stream_id, return_path, response).await
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        stream_id: u64,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        match payload {
            OnionCircuitPayload::HttpsResponse(response) => {
                self.https.complete_response(from, stream_id, response);
            }
            OnionCircuitPayload::HttpsError(message) => {
                self.https.complete_error(from, stream_id, message);
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) async fn execute_exit_fetch(
    runtime: &OnionHttpsRuntime,
    request: &OnionHttpsRequest,
    return_path: &[Did],
) -> Result<OnionHttpsResponse> {
    let target = OnionProxyTarget::parse_authority(&request.target)?;
    let authority = target.authority();
    let Some(policy) = runtime.exit_policy() else {
        return Err(Error::InvalidConfig(
            "browser HTTPS onion exit is not enabled locally".to_string(),
        ));
    };
    if !policy.allows_target(&authority) {
        return Err(Error::NoPermission);
    }
    let _lease = runtime.admit_exit_request(&policy, return_path, request.body.len() as u64)?;
    let url = format!("https://{}{}", authority, normalize_path(&request.path)?);
    let response = browser_fetch(&url, request, policy.max_bytes_per_minute).await?;
    runtime.record_exit_bytes(&policy, response.body.len() as u64)?;
    Ok(OnionHttpsResponse {
        status: response.status,
        headers: response.headers,
        body: response.body,
    })
}

struct FetchResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn browser_fetch(
    url: &str,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
) -> Result<FetchResponse> {
    let global = js_sys::global();
    let fetch = Reflect::get(global.as_ref(), JsValue::from_str("fetch").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let init = fetch_init(request)?;
    let response = JsFuture::from(Promise::from(
        fetch
            .call2(
                global.as_ref(),
                JsValue::from_str(url).as_ref(),
                init.as_ref(),
            )
            .map_err(js_error)?,
    ))
    .await
    .map_err(js_error)?;
    let status = Reflect::get(response.as_ref(), JsValue::from_str("status").as_ref())
        .map_err(js_error)?
        .as_f64()
        .ok_or_else(|| {
            Error::HttpRequestError("fetch response status is not numeric".to_string())
        })? as u16;
    let headers = collect_headers(&response)?;
    reject_content_length_over_limit(&headers, max_body_bytes)?;
    let body = response_body(&response, max_body_bytes).await?;
    Ok(FetchResponse {
        status,
        headers,
        body,
    })
}

fn fetch_init(request: &OnionHttpsRequest) -> Result<Object> {
    let init = Object::new();
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("method").as_ref(),
        JsValue::from_str(normalize_method(&request.method).as_str()).as_ref(),
    )
    .map_err(js_error)?;
    let headers = Object::new();
    for (name, value) in &request.headers {
        Reflect::set(
            headers.as_ref(),
            JsValue::from_str(name).as_ref(),
            JsValue::from_str(value).as_ref(),
        )
        .map_err(js_error)?;
    }
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("headers").as_ref(),
        headers.as_ref(),
    )
    .map_err(js_error)?;
    if !request.body.is_empty() {
        let body = Uint8Array::from(request.body.as_slice());
        Reflect::set(
            init.as_ref(),
            JsValue::from_str("body").as_ref(),
            body.as_ref(),
        )
        .map_err(js_error)?;
    }
    Ok(init)
}

fn collect_headers(response: &JsValue) -> Result<Vec<(String, String)>> {
    let headers =
        Reflect::get(response, JsValue::from_str("headers").as_ref()).map_err(js_error)?;
    let for_each = Reflect::get(headers.as_ref(), JsValue::from_str("forEach").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let pairs = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let pairs_for_callback = pairs.clone();
    let callback = Closure::wrap(Box::new(move |value: JsValue, name: JsValue| {
        if let (Some(name), Some(value)) = (name.as_string(), value.as_string()) {
            pairs_for_callback.borrow_mut().push((name, value));
        }
    }) as Box<dyn FnMut(JsValue, JsValue)>);
    for_each
        .call1(headers.as_ref(), callback.as_ref().unchecked_ref())
        .map_err(js_error)?;
    drop(callback);
    let collected = pairs.borrow().clone();
    Ok(collected)
}

fn reject_content_length_over_limit(
    headers: &[(String, String)],
    max_body_bytes: u64,
) -> Result<()> {
    if max_body_bytes == 0 {
        return Ok(());
    }
    let Some(length) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<u64>().ok())
    else {
        return Ok(());
    };
    if length > max_body_bytes {
        return Err(Error::NoPermission);
    }
    Ok(())
}

async fn response_body(response: &JsValue, max_body_bytes: u64) -> Result<Vec<u8>> {
    let body = Reflect::get(response, JsValue::from_str("body").as_ref()).map_err(js_error)?;
    if body.is_null() || body.is_undefined() {
        return Ok(Vec::new());
    }
    let get_reader = Reflect::get(body.as_ref(), JsValue::from_str("getReader").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let reader = get_reader.call0(body.as_ref()).map_err(js_error)?;
    let read = Reflect::get(reader.as_ref(), JsValue::from_str("read").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let cancel = Reflect::get(reader.as_ref(), JsValue::from_str("cancel").as_ref())
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok());
    let mut body = Vec::new();
    loop {
        let chunk = JsFuture::from(Promise::from(
            read.call0(reader.as_ref()).map_err(js_error)?,
        ))
        .await
        .map_err(js_error)?;
        let done = Reflect::get(chunk.as_ref(), JsValue::from_str("done").as_ref())
            .map_err(js_error)?
            .as_bool()
            .unwrap_or(false);
        if done {
            break;
        }
        let value =
            Reflect::get(chunk.as_ref(), JsValue::from_str("value").as_ref()).map_err(js_error)?;
        if value.is_null() || value.is_undefined() {
            continue;
        }
        let bytes = Uint8Array::new(value.as_ref()).to_vec();
        if max_body_bytes > 0
            && (body.len() as u64).saturating_add(bytes.len() as u64) > max_body_bytes
        {
            if let Some(cancel) = cancel {
                let _ = cancel.call0(reader.as_ref());
            }
            return Err(Error::NoPermission);
        }
        body.extend_from_slice(bytes.as_slice());
    }
    Ok(body)
}

fn normalize_method(method: &str) -> String {
    let method = method.trim();
    if method.is_empty() {
        default_method()
    } else {
        method.to_ascii_uppercase()
    }
}

fn normalize_path(path: &str) -> Result<String> {
    let path = path.trim();
    if path.is_empty() {
        return Ok(default_path());
    }
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    if path.starts_with('?') {
        return Ok(format!("/{path}"));
    }
    Err(Error::HttpRequestError(format!(
        "browser HTTPS onion proxy path must start with '/' or '?', got {path:?}"
    )))
}

fn default_method() -> String {
    "GET".to_string()
}

fn default_path() -> String {
    "/".to_string()
}

fn js_error(error: JsValue) -> Error {
    Error::JsError(format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;

    use super::*;

    fn did() -> Did {
        SecretKey::random().address().into()
    }

    #[test]
    fn normalizes_empty_request_defaults() {
        let request = OnionHttpsClientRequest {
            method: String::new(),
            path: Some(String::new()),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let target = OnionProxyTarget::parse_authority("Example.COM:443").unwrap();
        let wire =
            client_request_with_default_path(&target, request, default_path().as_str()).unwrap();

        assert_eq!(wire, OnionHttpsRequest {
            target: "example.com:443".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        });
    }

    #[test]
    fn client_request_from_url_uses_https_url_target_and_path() -> Result<()> {
        let (target, wire) = client_request_from_url(
            "https://Example.COM/search?q=rust#ignored",
            Default::default(),
        )?;

        assert_eq!(target.authority(), "example.com:443");
        assert_eq!(wire.target, "example.com:443");
        assert_eq!(wire.method, "GET");
        assert_eq!(wire.path, "/search?q=rust");
        Ok(())
    }

    #[test]
    fn client_request_from_url_preserves_explicit_port_and_path_override() -> Result<()> {
        let request = OnionHttpsClientRequest {
            path: Some("?override=1".to_string()),
            ..OnionHttpsClientRequest::default()
        };
        let (target, wire) = client_request_from_url("https://Example.COM:8443/original", request)?;

        assert_eq!(target.authority(), "example.com:8443");
        assert_eq!(wire.target, "example.com:8443");
        assert_eq!(wire.path, "/?override=1");
        Ok(())
    }

    #[test]
    fn client_request_from_url_rejects_non_https_urls() {
        assert!(matches!(
            client_request_from_url("http://example.com/", Default::default()),
            Err(Error::HttpRequestError(_))
        ));
    }

    #[test]
    fn rejects_relative_path_without_slash() {
        assert!(matches!(
            normalize_path("index.html"),
            Err(Error::HttpRequestError(_))
        ));
    }

    #[test]
    fn cancel_request_removes_pending_request() {
        let runtime = OnionHttpsRuntime::new();
        let (id, _receiver) = runtime.begin_request(did()).unwrap();

        assert_eq!(runtime.pending_len(), 1);
        runtime.cancel_request(id);
        assert_eq!(runtime.pending_len(), 0);
    }

    #[test]
    fn pending_request_completes_only_from_expected_return_peer() {
        let runtime = OnionHttpsRuntime::new();
        let expected = did();
        let other = did();
        let (id, receiver) = runtime.begin_request(expected).unwrap();

        runtime.complete_error(other, id, "wrong peer".to_string());
        assert_eq!(runtime.pending_len(), 1);
        drop(receiver);
        runtime.cancel_request(id);
    }

    #[test]
    fn exit_limiter_rejects_bytes_over_policy_window() {
        let runtime = OnionHttpsRuntime::new();
        let policy = OnionExitPolicy {
            max_bytes_per_minute: 8,
            ..OnionExitPolicy::default()
        };
        let return_path = vec![did()];
        let _lease = runtime
            .admit_exit_request(&policy, &return_path, 4)
            .unwrap();

        assert!(runtime.record_exit_bytes(&policy, 4).is_ok());
        assert!(matches!(
            runtime.record_exit_bytes(&policy, 1),
            Err(Error::NoPermission)
        ));
    }

    #[test]
    fn exit_limiter_enforces_streams_per_circuit() {
        let runtime = OnionHttpsRuntime::new();
        let policy = OnionExitPolicy {
            max_streams_per_circuit: 1,
            ..OnionExitPolicy::default()
        };
        let return_path = vec![did(), did()];

        let lease = runtime
            .admit_exit_request(&policy, &return_path, 0)
            .expect("first stream admitted");
        assert!(matches!(
            runtime.admit_exit_request(&policy, &return_path, 0),
            Err(Error::NoPermission)
        ));
        drop(lease);
        assert!(runtime.admit_exit_request(&policy, &return_path, 0).is_ok());
    }

    #[test]
    fn circuit_protocol_install_marker_starts_false_then_marks_true() {
        let runtime = OnionHttpsRuntime::new();

        assert!(!runtime.circuit_protocol_installed());
        runtime.mark_circuit_protocol_installed();
        assert!(runtime.circuit_protocol_installed());
    }
}
