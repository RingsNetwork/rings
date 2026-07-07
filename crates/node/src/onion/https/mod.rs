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
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use futures::channel::oneshot;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use rings_core::session::SessionSk;
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
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionCircuitExitFrame;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::circuit::OnionReturnId;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::proxy::ONION_PROXY_HTTPS_SERVICE;
use crate::onion::replay::OnionForwardReplayCache;
use crate::onion::replay::OnionForwardReplayKey;
use crate::onion::replay::ReplayAdmission;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionRouteError;

const DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

/// One browser HTTPS request executed by an HTTPS exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionHttpsRequest {
    /// Target authority (`host:port`).
    pub target: String,
    /// HTTP method.
    pub method: String,
    /// Path and query.
    pub path: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Request body bytes.
    pub body: Vec<u8>,
}

/// One browser HTTPS response returned by an HTTPS exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionHttpsResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum OnionHttpsPayload {
    Request(OnionHttpsRequest),
    Response(OnionHttpsResponse),
    Error(OnionExitFailure),
}

pub(crate) fn encode_https_payload(payload: OnionHttpsPayload) -> Result<OnionCircuitPayload> {
    bincode::serialize(&payload)
        .map(|body| {
            OnionCircuitPayload::new(crate::onion::OnionServiceName::https(), Bytes::from(body))
        })
        .map_err(|_| Error::EncodeError)
}

fn decode_https_payload(payload: OnionCircuitPayload) -> Result<Option<OnionHttpsPayload>> {
    if !payload.matches_service(ONION_PROXY_HTTPS_SERVICE) {
        return Ok(None);
    }
    bincode::deserialize(payload.body.as_ref())
        .map(Some)
        .map_err(|_| Error::DecodeError)
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
    pending: Mutex<HashMap<OnionCircuitId, PendingRequest>>,
    exit_policy: Mutex<Option<OnionExitPolicy>>,
    forward_replays: Mutex<OnionForwardReplayCache>,
    accounting: OnionExitAccounting,
}

struct PendingRequest {
    expected_return_peer: Did,
    expected_exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    sender: oneshot::Sender<std::result::Result<OnionHttpsClientResponse, Error>>,
}

impl OnionHttpsRuntime {
    /// Create an empty runtime.
    pub(crate) fn new() -> Self {
        Self::default()
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
        expected_exit: OnionExitDescriptor,
        return_id: OnionReturnId,
    ) -> Result<(
        OnionCircuitId,
        oneshot::Receiver<std::result::Result<OnionHttpsClientResponse, Error>>,
    )> {
        let mut pending = self.pending.lock().map_err(|_| Error::Lock)?;
        for _ in 0..16 {
            let id = OnionCircuitId::random();
            if pending.contains_key(&id) {
                continue;
            }
            let (sender, receiver) = oneshot::channel();
            pending.insert(id, PendingRequest {
                expected_return_peer,
                expected_exit,
                return_id,
                sender,
            });
            return Ok((id, receiver));
        }
        Err(Error::OnionRouteError(
            OnionRouteError::CircuitIdAllocationFailed,
        ))
    }

    /// Cancel a request that failed before it was sent.
    pub(crate) fn cancel_request(&self, id: OnionCircuitId) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// Complete a pending HTTPS request with a signed response or error payload.
    pub(crate) fn complete_payload(
        &self,
        from: Did,
        id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) {
        let Some((pending, payload)) = self.take_pending_payload(from, id, payload) else {
            return;
        };
        match decode_https_payload(payload) {
            Ok(Some(OnionHttpsPayload::Response(response))) => {
                let _ = pending.sender.send(Ok(OnionHttpsClientResponse {
                    status: response.status,
                    headers: response.headers,
                    body: response.body,
                }));
            }
            Ok(Some(OnionHttpsPayload::Error(failure))) => {
                let _ =
                    pending
                        .sender
                        .send(Err(Error::OnionRouteError(OnionRouteError::ExitFailure(
                            failure,
                        ))));
            }
            Ok(Some(OnionHttpsPayload::Request(_)) | None) => {
                let _ = pending.sender.send(Err(Error::OnionRouteError(
                    OnionRouteError::UnexpectedBackwardPayload,
                )));
            }
            Err(error) => {
                let _ = pending.sender.send(Err(error));
            }
        }
    }

    fn take_pending_payload(
        &self,
        from: Did,
        id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Option<(PendingRequest, OnionCircuitPayload)> {
        let mut pending = self.pending.lock().ok()?;
        let request = pending.remove(&id)?;
        if request.expected_return_peer != from {
            pending.insert(id, request);
            return None;
        }
        match payload.into_verified_payload(request.return_id, &request.expected_exit) {
            Ok(verified) => Some((request, verified.payload)),
            Err(error) => {
                let _ = request.sender.send(Err(error));
                None
            }
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
        circuit_id: OnionCircuitId,
        return_peer: Did,
        bytes: u64,
    ) -> Result<OnionExitLease> {
        self.accounting
            .admit(policy, circuit_id, return_peer, bytes)
    }

    fn record_exit_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        self.accounting.record_bytes(policy, bytes)
    }

    fn remaining_exit_bytes(&self, policy: &OnionExitPolicy) -> Result<Option<u64>> {
        self.accounting.remaining_bytes(policy)
    }

    fn consume_forward_nonce(
        &self,
        circuit_id: OnionCircuitId,
        nonce: OnionForwardNonce,
    ) -> Result<()> {
        let mut replays = self.forward_replays.lock().map_err(|_| Error::Lock)?;
        match replays.consume(
            OnionForwardReplayKey::new(circuit_id, nonce),
            rings_core::utils::get_epoch_ms(),
        ) {
            ReplayAdmission::Consumed => Ok(()),
            ReplayAdmission::Duplicate => {
                Err(Error::OnionRouteError(OnionRouteError::ForwardReplay))
            }
            ReplayAdmission::Full => Err(Error::NoPermission),
        }
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
    session_sk: SessionSk,
}

impl BrowserOnionCircuitHandler {
    /// Create a browser circuit handler backed by the HTTPS runtime.
    pub(crate) fn new(https: Arc<OnionHttpsRuntime>, session_sk: SessionSk) -> Self {
        Self { https, session_sk }
    }
}

#[async_trait::async_trait(?Send)]
impl OnionCircuitHandler for BrowserOnionCircuitHandler {
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()> {
        let Some(payload) = decode_https_payload(frame.payload)? else {
            return Ok(());
        };
        let response = match payload {
            OnionHttpsPayload::Request(request) => {
                match execute_exit_fetch(
                    &self.https,
                    &request,
                    frame.circuit_id,
                    frame.return_peer,
                    frame.forward_nonce,
                )
                .await
                {
                    Ok(response) => OnionHttpsPayload::Response(response),
                    Err(error) => OnionHttpsPayload::Error(OnionExitFailure::from_error(&error)),
                }
            }
            OnionHttpsPayload::Response(_) | OnionHttpsPayload::Error(_) => return Ok(()),
        };
        send_backward(
            scope,
            &self.session_sk,
            frame.circuit_id,
            frame.return_peer,
            frame.client,
            encode_https_payload(response)?,
        )
        .await
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Result<()> {
        self.https.complete_payload(from, circuit_id, payload);
        Ok(())
    }
}

pub(crate) async fn execute_exit_fetch(
    runtime: &OnionHttpsRuntime,
    request: &OnionHttpsRequest,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    forward_nonce: OnionForwardNonce,
) -> Result<OnionHttpsResponse> {
    runtime.consume_forward_nonce(circuit_id, forward_nonce)?;
    let target = OnionProxyTarget::parse_authority(&request.target)?;
    let authority = target.authority();
    let exit_target = OnionExitTarget::from_proxy_target(&target);
    let Some(policy) = runtime.exit_policy() else {
        return Err(Error::InvalidConfig(
            "browser HTTPS onion exit is not enabled locally".to_string(),
        ));
    };
    if !policy.allows_target(&exit_target) {
        return Err(Error::NoPermission);
    }
    let request_body_bytes = usize_to_u64(request.body.len())?;
    let _lease =
        runtime.admit_exit_request(&policy, circuit_id, return_peer, request_body_bytes)?;
    let body_limit = https_response_body_limit(runtime.remaining_exit_bytes(&policy)?);
    if body_limit == 0 {
        return Err(Error::NoPermission);
    }
    let url = format!("https://{}{}", authority, normalize_path(&request.path)?);
    let response = browser_fetch(&url, request, body_limit).await?;
    runtime.record_exit_bytes(&policy, usize_to_u64(response.body.len())?)?;
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
        .ok_or_else(|| Error::HttpRequestError("fetch response status is not numeric".to_string()))
        .and_then(checked_status_code)?;
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
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("credentials").as_ref(),
        JsValue::from_str("omit").as_ref(),
    )
    .map_err(js_error)?;
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("referrerPolicy").as_ref(),
        JsValue::from_str("no-referrer").as_ref(),
    )
    .map_err(js_error)?;
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("redirect").as_ref(),
        JsValue::from_str("error").as_ref(),
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

fn https_response_body_limit(remaining_policy_bytes: Option<u64>) -> u64 {
    remaining_policy_bytes.unwrap_or(DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES)
}

fn checked_status_code(status: f64) -> Result<u16> {
    if !status.is_finite() || status.fract() != 0.0 || !(100.0..=999.0).contains(&status) {
        return Err(Error::HttpRequestError(format!(
            "fetch response status is not a valid HTTP status: {status:?}"
        )));
    }
    status
        .to_string()
        .parse::<u16>()
        .map_err(|_| Error::HttpRequestError(format!("invalid fetch response status {status:?}")))
}

fn usize_to_u64(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidData)
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
        let body_len = usize_to_u64(body.len())?;
        let bytes_len = usize_to_u64(bytes.len())?;
        if max_body_bytes > 0 && body_len.saturating_add(bytes_len) > max_body_bytes {
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
mod tests;
