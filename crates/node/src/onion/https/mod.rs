//! HTTPS onion-exit request/response adapter.
//!
//! This protocol is intentionally application-layer HTTPS. Clients can send an HTTPS request
//! description over the route-aware onion circuit, the exit performs the request, and the response
//! is sent back over the circuit return path. The client half lives in the `client` submodule, shared by
//! native and browser callers; this module owns the wire payloads and the exit half.
//!
//! A browser page exit is constrained by the host browser's `fetch` capability: CORS, forbidden
//! headers, credentials policy, and extension host permissions still apply. A full arbitrary HTTPS
//! exit must run in a browser-extension or native context that grants those fetch permissions.

use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::message::MessageSigner;
use serde::Deserialize;
use serde::Serialize;

#[cfg(rings_browser)]
use self::browser::execute_https_request;
pub use self::client::OnionHttpsCall;
pub(crate) use self::client::OnionHttpsClient;
pub use self::client::OnionHttpsClientRequest;
#[cfg(test)]
use self::limits::checked_status_code;
use self::limits::https_response_body_limit;
use self::limits::usize_to_u64;
#[cfg(rings_native)]
use self::native::execute_https_request;
#[cfg(all(test, rings_native))]
use self::native::is_native_transport_managed_header;
#[cfg(all(test, rings_native))]
use self::native::native_fetch_with_timeout;
#[cfg(all(test, rings_native))]
use self::native::select_native_https_egress;
#[cfg(all(test, rings_native))]
use self::native::NativeHttpsEgress;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::send_backward;
#[cfg(rings_browser)]
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
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::proxy::ONION_PROXY_HTTPS_SERVICE;
use crate::onion::replay::OnionForwardReplayWitness;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionRouteError;

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
enum OnionHttpsPayload {
    Request(OnionHttpsRequest),
    Response(OnionHttpsResponse),
    Error(OnionExitFailure),
}

fn encode_https_payload(payload: OnionHttpsPayload) -> Result<OnionCircuitPayload> {
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

/// Shared runtime for the local HTTPS proxy protocol: the exit adapter and the node's client.
pub(crate) struct OnionHttpsRuntime {
    client: Arc<OnionHttpsClient>,
    exit_policy: Mutex<Option<OnionExitPolicy>>,
    /// Replay authority shared with the TCP adapter installed on this node.
    pub(super) forward_replays: OnionForwardReplayWitness,
    accounting: OnionExitAccounting,
    link_sender: OnionLinkSender,
    #[cfg(rings_native)]
    native_proxy: Mutex<Option<String>>,
}

impl OnionHttpsRuntime {
    /// Create a runtime with private accounting, link and replay resources.
    #[cfg(any(test, rings_browser))]
    pub(crate) fn new(return_key: PublicKey<33>) -> Self {
        Self::with_resources(
            return_key,
            OnionExitAccounting::default(),
            OnionLinkSender::default(),
            OnionForwardReplayWitness::default(),
        )
    }

    /// Create a runtime sharing node-wide accounting and link-traffic effect capabilities.
    ///
    /// `return_key` is the local delegatee key the client asks exits to answer to.
    pub(crate) fn with_resources(
        return_key: PublicKey<33>,
        accounting: OnionExitAccounting,
        link_sender: OnionLinkSender,
        forward_replays: OnionForwardReplayWitness,
    ) -> Self {
        Self {
            client: Arc::new(OnionHttpsClient::new(return_key, link_sender.clone())),
            exit_policy: Mutex::new(None),
            forward_replays,
            accounting,
            link_sender,
            #[cfg(rings_native)]
            native_proxy: Mutex::new(None),
        }
    }

    /// The node's HTTPS onion client.
    pub(crate) const fn client(&self) -> &Arc<OnionHttpsClient> {
        &self.client
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
}

/// Browser handler for HTTPS onion circuits.
#[cfg(rings_browser)]
pub(crate) struct BrowserOnionCircuitHandler {
    https: Arc<OnionHttpsRuntime>,
    /// The exit's signing authority for backward payloads.
    signer: MessageSigner<DelegateeKey>,
}

#[cfg(rings_browser)]
impl BrowserOnionCircuitHandler {
    /// Create a browser circuit handler backed by the HTTPS runtime, signing backward payloads
    /// for the overlay `network_id`.
    pub(crate) fn new(https: Arc<OnionHttpsRuntime>, signer: MessageSigner<DelegateeKey>) -> Self {
        Self { https, signer }
    }
}

#[cfg(rings_browser)]
#[async_trait::async_trait(?Send)]
impl OnionCircuitHandler for BrowserOnionCircuitHandler {
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()> {
        let _ =
            try_handle_https_exit_payload(&self.https, self.signer.by_ref(), scope, frame).await?;
        Ok(())
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Result<()> {
        // No other client adapter shares the browser circuit protocol, so an unclaimed payload is
        // a late reply to a cancelled or timed-out request, or a misrouted one.
        match self.https.client().claim(from, circuit_id)? {
            Some(claim) => claim.resolve(payload, self.signer.network_id()),
            None => tracing::debug!(%from, "dropping unclaimed onion HTTPS backward payload"),
        }
        Ok(())
    }
}

pub(crate) async fn try_handle_https_exit_payload(
    runtime: &Arc<OnionHttpsRuntime>,
    signer: MessageSigner<&DelegateeKey>,
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
        signer,
        OnionBackwardPath::new(
            frame.circuit_id,
            frame.return_peer,
            frame.return_delegatee_public_key,
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
    runtime
        .forward_replays
        .consume_forward_nonce(return_peer, circuit_id, forward_nonce)?;
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
        "HTTPS onion proxy path must start with '/' or '?', got {path:?}"
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
mod client;
mod limits;
#[cfg(rings_native)]
mod native;
mod pending;
