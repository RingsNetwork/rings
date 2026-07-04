#![warn(missing_docs)]
//! Browser HTTPS onion-exit request/response data plane.
//!
//! This protocol is intentionally application-layer HTTPS. Browser exits cannot expose raw TCP,
//! so a client sends an HTTPS request description to the selected exit, the exit performs
//! `fetch`, and the response is sent back over the same namespace.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Ctx;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::onion::OnionExitPolicy;
use crate::onion_proxy::OnionProxyTarget;

/// Namespace used by the browser HTTPS onion proxy protocol.
pub const ONION_HTTPS_PROXY_NAMESPACE: &str = "onion-https";

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
    /// Path and query to request on the target authority. Defaults to `/`.
    #[serde(default = "default_path")]
    pub path: String,
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
            path: default_path(),
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

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) struct OnionHttpsRequest {
    id: u64,
    target: String,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) struct OnionHttpsResponse {
    id: u64,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) struct OnionHttpsError {
    id: u64,
    message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) enum OnionHttpsMessage {
    Request(OnionHttpsRequest),
    Response(OnionHttpsResponse),
    Error(OnionHttpsError),
}

/// Shared runtime for the local browser HTTPS proxy protocol.
#[derive(Default)]
pub(crate) struct OnionHttpsRuntime {
    next_request: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    exit_policy: Mutex<Option<OnionExitPolicy>>,
}

struct PendingRequest {
    exit: Did,
    sender: oneshot::Sender<std::result::Result<OnionHttpsClientResponse, String>>,
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

    /// Begin a client request expected to complete from `exit`.
    pub(crate) fn begin_request(
        &self,
        exit: Did,
    ) -> Result<(
        u64,
        oneshot::Receiver<std::result::Result<OnionHttpsClientResponse, String>>,
    )> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| Error::Lock)?
            .insert(id, PendingRequest { exit, sender });
        Ok((id, receiver))
    }

    /// Cancel a request that failed before it was sent.
    pub(crate) fn cancel_request(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    fn complete_response(&self, from: Did, response: OnionHttpsResponse) {
        let Some(pending) = self.take_pending(from, response.id) else {
            return;
        };
        let _ = pending.sender.send(Ok(OnionHttpsClientResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        }));
    }

    fn complete_error(&self, from: Did, error: OnionHttpsError) {
        let Some(pending) = self.take_pending(from, error.id) else {
            return;
        };
        let _ = pending.sender.send(Err(error.message));
    }

    fn take_pending(&self, from: Did, id: u64) -> Option<PendingRequest> {
        let mut pending = self.pending.lock().ok()?;
        let request = pending.remove(&id)?;
        if request.exit == from {
            Some(request)
        } else {
            pending.insert(id, request);
            None
        }
    }

    fn exit_policy(&self) -> Option<OnionExitPolicy> {
        self.exit_policy
            .lock()
            .ok()
            .and_then(|policy| policy.clone())
    }
}

/// Browser HTTPS onion proxy protocol.
pub(crate) struct OnionHttpsProtocol;

pub(crate) struct OnionHttpsEvent {
    from: Did,
    message: OnionHttpsMessage,
}

/// Effects interpreted by [`OnionHttpsShell`].
pub(crate) enum OnionHttpsEffect {
    /// Execute a local browser fetch and reply to the requester.
    FetchAndReply {
        /// Requester DID.
        to: Did,
        /// Fetch request.
        request: OnionHttpsRequest,
    },
    /// Complete a pending local client request.
    CompleteResponse {
        /// Exit DID.
        from: Did,
        /// Response from the exit.
        response: OnionHttpsResponse,
    },
    /// Complete a pending local client request with an error.
    CompleteError {
        /// Exit DID.
        from: Did,
        /// Error from the exit.
        error: OnionHttpsError,
    },
}

impl Protocol for OnionHttpsProtocol {
    type State = ();
    type Event = OnionHttpsEvent;
    type Effect = OnionHttpsEffect;

    fn namespace(&self) -> &str {
        ONION_HTTPS_PROXY_NAMESPACE
    }

    fn init(&self) -> Self::State {}

    fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
        let message = bincode::deserialize::<OnionHttpsMessage>(wire.payload)
            .map_err(|error| Reject(format!("bad onion HTTPS proxy message: {error}")))?;
        Ok(OnionHttpsEvent {
            from: wire.from,
            message,
        })
    }

    fn step(
        &self,
        _ctx: Ctx<'_, Self::State>,
        event: Self::Event,
    ) -> Transition<Self::State, Self::Effect> {
        let effect = match event.message {
            OnionHttpsMessage::Request(request) => OnionHttpsEffect::FetchAndReply {
                to: event.from,
                request,
            },
            OnionHttpsMessage::Response(response) => OnionHttpsEffect::CompleteResponse {
                from: event.from,
                response,
            },
            OnionHttpsMessage::Error(error) => OnionHttpsEffect::CompleteError {
                from: event.from,
                error,
            },
        };
        Transition::with((), vec![effect])
    }
}

/// Browser HTTPS onion proxy interpreter.
pub(crate) struct OnionHttpsShell {
    runtime: Arc<OnionHttpsRuntime>,
}

impl OnionHttpsShell {
    /// Create an interpreter backed by `runtime`.
    pub(crate) fn new(runtime: Arc<OnionHttpsRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait(?Send)]
impl Interpret for OnionHttpsShell {
    type Effect = OnionHttpsEffect;

    async fn run(&self, scope: &Scope, effect: OnionHttpsEffect) -> Result<Vec<Bytes>> {
        match effect {
            OnionHttpsEffect::FetchAndReply { to, request } => {
                let response = execute_exit_fetch(self.runtime.exit_policy(), &request).await;
                let message = match response {
                    Ok(response) => OnionHttpsMessage::Response(response),
                    Err(error) => OnionHttpsMessage::Error(OnionHttpsError {
                        id: request.id,
                        message: error.to_string(),
                    }),
                };
                let payload = bincode::serialize(&message).map_err(|_| Error::EncodeError)?;
                scope.send(to, Bytes::from(payload)).await?;
            }
            OnionHttpsEffect::CompleteResponse { from, response } => {
                self.runtime.complete_response(from, response);
            }
            OnionHttpsEffect::CompleteError { from, error } => {
                self.runtime.complete_error(from, error);
            }
        }
        Ok(Vec::new())
    }
}

/// Encode one client request for the selected exit.
pub(crate) fn encode_request(
    id: u64,
    target: &OnionProxyTarget,
    request: OnionHttpsClientRequest,
) -> Result<Bytes> {
    let message = OnionHttpsMessage::Request(OnionHttpsRequest {
        id,
        target: target.authority(),
        method: normalize_method(&request.method),
        path: normalize_path(&request.path)?,
        headers: request.headers,
        body: request.body,
    });
    bincode::serialize(&message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

async fn execute_exit_fetch(
    policy: Option<OnionExitPolicy>,
    request: &OnionHttpsRequest,
) -> Result<OnionHttpsResponse> {
    let target = OnionProxyTarget::parse_authority(&request.target)?;
    let authority = target.authority();
    let Some(policy) = policy else {
        return Err(Error::InvalidConfig(
            "browser HTTPS onion exit is not enabled locally".to_string(),
        ));
    };
    if !policy.allows_target(&authority) {
        return Err(Error::NoPermission);
    }
    let url = format!("https://{}{}", authority, normalize_path(&request.path)?);
    let response = browser_fetch(&url, request).await?;
    Ok(OnionHttpsResponse {
        id: request.id,
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

async fn browser_fetch(url: &str, request: &OnionHttpsRequest) -> Result<FetchResponse> {
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
    let body = response_body(&response).await?;
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

async fn response_body(response: &JsValue) -> Result<Vec<u8>> {
    let array_buffer = Reflect::get(response, JsValue::from_str("arrayBuffer").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let buffer = JsFuture::from(Promise::from(
        array_buffer.call0(response).map_err(js_error)?,
    ))
    .await
    .map_err(js_error)?;
    Ok(Uint8Array::new(buffer.as_ref()).to_vec())
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
    use super::*;

    #[test]
    fn normalizes_empty_request_defaults() {
        let request = OnionHttpsClientRequest {
            method: String::new(),
            path: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let target = OnionProxyTarget::parse_authority("Example.COM:443").unwrap();
        let payload = encode_request(7, &target, request).unwrap();
        let decoded = bincode::deserialize::<OnionHttpsMessage>(&payload).unwrap();

        assert_eq!(
            decoded,
            OnionHttpsMessage::Request(OnionHttpsRequest {
                id: 7,
                target: "example.com:443".to_string(),
                method: "GET".to_string(),
                path: "/".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            })
        );
    }

    #[test]
    fn rejects_relative_path_without_slash() {
        assert!(matches!(
            normalize_path("index.html"),
            Err(Error::HttpRequestError(_))
        ));
    }
}
