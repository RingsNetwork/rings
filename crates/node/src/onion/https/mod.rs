//! HTTPS onion-exit request/response adapter.
//!
//! This protocol is intentionally application-layer HTTPS. Clients can send an HTTPS request
//! description over the route-aware onion circuit, the exit performs the request, and the response
//! is sent back over the circuit return path.
//!
//! A browser page exit is constrained by the host browser's `fetch` capability: CORS, forbidden
//! headers, credentials policy, and extension host permissions still apply. A full arbitrary HTTPS
//! exit must run in a browser-extension or native context that grants those fetch permissions.

#[cfg(any(test, rings_browser))]
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
#[cfg(any(test, rings_browser))]
use futures::channel::oneshot;
use rings_core::dht::Did;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;

#[cfg(rings_browser)]
use self::browser::execute_https_request;
#[cfg(test)]
use self::limits::checked_status_code;
use self::limits::https_response_body_limit;
use self::limits::usize_to_u64;
#[cfg(rings_native)]
use self::native::execute_https_request;
#[cfg(all(test, rings_native))]
use self::native::native_fetch_with_timeout;
#[cfg(all(test, rings_native))]
use self::native::select_native_https_egress;
#[cfg(all(test, rings_native))]
use self::native::NativeHttpsEgress;
#[cfg(any(test, rings_browser))]
use self::pending::PendingOnionHttpsRequest;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::send_backward;
#[cfg(any(test, rings_browser))]
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionBackwardPath;
use crate::onion::circuit::OnionBackwardSequence;
use crate::onion::circuit::OnionCircuitExitFrame;
#[cfg(rings_browser)]
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::circuit::OnionForwardSequence;
use crate::onion::circuit::OnionLinkSender;
#[cfg(any(test, rings_browser))]
use crate::onion::circuit::OnionReturnId;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::proxy::ONION_PROXY_HTTPS_SERVICE;
use crate::onion::replay::OnionForwardReplayKey;
use crate::onion::replay::OnionForwardReplayPartitions;
use crate::onion::replay::ReplayAdmission;
#[cfg(any(test, rings_browser))]
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionRouteError;
use crate::sync_lock::lock;

const DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

/// One HTTPS request executed by an HTTPS exit.
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

/// One HTTPS response returned by an HTTPS exit.
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
    rings_codec::serialize(&payload)
        .map(|body| {
            OnionCircuitPayload::new(crate::onion::OnionServiceName::https(), Bytes::from(body))
        })
        .map_err(|_| Error::EncodeError)
}

fn decode_https_payload(payload: OnionCircuitPayload) -> Result<Option<OnionHttpsPayload>> {
    if !payload.matches_service(ONION_PROXY_HTTPS_SERVICE) {
        return Ok(None);
    }
    rings_codec::deserialize(payload.body.as_ref())
        .map(Some)
        .map_err(|_| Error::DecodeError)
}

/// JS-facing request fields for one HTTPS proxy request.
#[cfg(any(test, rings_browser))]
#[cfg_attr(test, derive(Default))]
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

/// JS-facing response fields returned from one HTTPS proxy request.
#[cfg(any(test, rings_browser))]
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct OnionHttpsClientResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Shared runtime for the local HTTPS proxy protocol.
pub(crate) struct OnionHttpsRuntime {
    #[cfg(any(test, rings_browser))]
    pending: Mutex<HashMap<OnionCircuitId, PendingRequest>>,
    exit_policy: Mutex<Option<OnionExitPolicy>>,
    forward_replays: Mutex<OnionForwardReplayPartitions>,
    accounting: OnionExitAccounting,
    link_sender: OnionLinkSender,
    #[cfg(rings_native)]
    native_proxy: Mutex<Option<String>>,
}

impl Default for OnionHttpsRuntime {
    fn default() -> Self {
        Self::with_resources(OnionExitAccounting::default(), OnionLinkSender::default())
    }
}

#[cfg(any(test, rings_browser))]
struct PendingRequest {
    expected_return_peer: Did,
    expected_exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    sender: oneshot::Sender<std::result::Result<OnionHttpsClientResponse, Error>>,
}

impl OnionHttpsRuntime {
    /// Create an empty runtime.
    #[cfg(any(test, rings_browser))]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Create a runtime sharing node-wide accounting and link-traffic effect capabilities.
    pub(crate) fn with_resources(
        accounting: OnionExitAccounting,
        link_sender: OnionLinkSender,
    ) -> Self {
        Self {
            #[cfg(any(test, rings_browser))]
            pending: Mutex::new(HashMap::new()),
            exit_policy: Mutex::new(None),
            forward_replays: Mutex::new(OnionForwardReplayPartitions::default()),
            accounting,
            link_sender,
            #[cfg(rings_native)]
            native_proxy: Mutex::new(None),
        }
    }

    #[cfg(rings_browser)]
    pub(crate) fn link_sender(&self) -> OnionLinkSender {
        self.link_sender.clone()
    }

    #[cfg(rings_native)]
    pub(crate) fn set_native_proxy(&self, proxy: Option<String>) {
        if let Ok(mut current) = self.native_proxy.lock() {
            *current = proxy;
        }
    }

    #[cfg(rings_native)]
    pub(crate) fn native_proxy(&self) -> Option<String> {
        self.native_proxy
            .lock()
            .ok()
            .and_then(|proxy| proxy.clone())
    }

    #[cfg(all(test, rings_native))]
    pub(crate) fn accounting_for_test(&self) -> OnionExitAccounting {
        self.accounting.clone()
    }

    /// Set the local exit policy. `None` means client-only mode.
    pub(crate) fn set_exit_policy(&self, policy: Option<OnionExitPolicy>) {
        if let Ok(mut current) = self.exit_policy.lock() {
            *current = policy;
        }
    }

    /// Begin a client request expected to complete from the immediate return peer.
    #[cfg(any(test, rings_browser))]
    pub(crate) fn begin_request(
        self: &Arc<Self>,
        expected_return_peer: Did,
        expected_exit: OnionExitDescriptor,
        return_id: OnionReturnId,
    ) -> Result<(OnionCircuitId, PendingOnionHttpsRequest)> {
        let mut pending = lock(&self.pending)?;
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
            return Ok((
                id,
                PendingOnionHttpsRequest::new(self.clone(), id, receiver),
            ));
        }
        Err(Error::OnionRouteError(
            OnionRouteError::CircuitIdAllocationFailed,
        ))
    }

    #[cfg(any(test, rings_browser))]
    fn cancel_request(&self, id: OnionCircuitId) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// Complete a pending HTTPS request with a signed response or error payload.
    #[cfg(any(test, rings_browser))]
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

    #[cfg(any(test, rings_browser))]
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
        from: Did,
        circuit_id: OnionCircuitId,
        nonce: OnionForwardNonce,
    ) -> Result<()> {
        let mut replays = lock(&self.forward_replays)?;
        match replays.consume(
            from,
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
    pub(crate) fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }
}

/// Parse a full HTTPS URL and encode one client request for its target.
#[cfg(any(test, rings_browser))]
pub(crate) fn client_request_from_url(
    url: &str,
    request: OnionHttpsClientRequest,
) -> Result<(OnionProxyTarget, OnionHttpsRequest)> {
    let (target, path) = parse_https_url(url)?;
    let request = client_request_with_default_path(&target, request, path.as_str())?;
    Ok((target, request))
}

#[cfg(any(test, rings_browser))]
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

#[cfg(any(test, rings_browser))]
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

#[cfg(any(test, rings_browser))]
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

#[cfg(any(test, rings_browser))]
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
#[cfg(rings_browser)]
pub(crate) struct BrowserOnionCircuitHandler {
    https: Arc<OnionHttpsRuntime>,
    session_sk: SessionSk,
}

#[cfg(rings_browser)]
impl BrowserOnionCircuitHandler {
    /// Create a browser circuit handler backed by the HTTPS runtime.
    pub(crate) fn new(https: Arc<OnionHttpsRuntime>, session_sk: SessionSk) -> Self {
        Self { https, session_sk }
    }
}

#[cfg(rings_browser)]
#[async_trait::async_trait(?Send)]
impl OnionCircuitHandler for BrowserOnionCircuitHandler {
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()> {
        let _ = try_handle_https_exit_payload(&self.https, &self.session_sk, scope, frame).await?;
        Ok(())
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

pub(crate) async fn try_handle_https_exit_payload(
    runtime: &Arc<OnionHttpsRuntime>,
    session_sk: &SessionSk,
    scope: &Scope,
    frame: OnionCircuitExitFrame,
) -> Result<bool> {
    if !frame.payload.matches_service(ONION_PROXY_HTTPS_SERVICE) {
        return Ok(false);
    }
    let Some(payload) = (match decode_https_payload(frame.payload) {
        Ok(payload) => payload,
        Err(Error::DecodeError) => return Ok(false),
        Err(error) => return Err(error),
    }) else {
        return Ok(false);
    };
    let response = match payload {
        OnionHttpsPayload::Request(request) => {
            match execute_exit_fetch(
                runtime,
                &request,
                frame.circuit_id,
                frame.return_peer,
                frame.forward_nonce,
                frame.forward_sequence,
            )
            .await
            {
                Ok(response) => OnionHttpsPayload::Response(response),
                Err(error) => OnionHttpsPayload::Error(OnionExitFailure::from_error(&error)),
            }
        }
        OnionHttpsPayload::Response(_) | OnionHttpsPayload::Error(_) => return Ok(true),
    };
    send_backward(
        &runtime.link_sender,
        scope,
        session_sk,
        OnionBackwardPath::new(
            frame.circuit_id,
            frame.return_peer,
            frame.return_session_public_key,
            frame.client,
        ),
        OnionBackwardSequence::FIRST,
        encode_https_payload(response)?,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn execute_exit_fetch(
    runtime: &OnionHttpsRuntime,
    request: &OnionHttpsRequest,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    forward_nonce: OnionForwardNonce,
    forward_sequence: OnionForwardSequence,
) -> Result<OnionHttpsResponse> {
    if forward_sequence != OnionForwardSequence::FIRST {
        return Err(Error::OnionRouteError(OnionRouteError::ForwardReplay));
    }
    runtime.consume_forward_nonce(return_peer, circuit_id, forward_nonce)?;
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
    let response =
        execute_https_request(&url, &target, request, body_limit, runtime, &policy).await?;
    Ok(OnionHttpsResponse {
        status: response.status,
        headers: response.headers,
        body: response.body,
    })
}

pub(super) struct FetchResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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

#[cfg(test)]
mod tests;

#[cfg(rings_browser)]
mod browser;
mod limits;
#[cfg(rings_native)]
mod native;
#[cfg(any(test, rings_browser))]
mod pending;
