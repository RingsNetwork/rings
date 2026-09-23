//! Client half of the HTTPS onion protocol, shared by native and browser callers.
//!
//! One request owns one circuit. Route selection is the caller's platform decision (a browser must
//! start at a direct peer and may read a remote directory); everything after it is target
//! independent:
//!
//! ```text
//! url ──client_request_from_url──▶ (target, request)
//!     ──route(target)──────────▶ OnionProxyRoute                      [caller]
//!     ──begin──────────────────▶ OnionHttpsFlight {link, cell, guard} [pending table]
//!     ──send_sealed────────────▶ first hop                            [link effect]
//!     ──within(deadline)───────▶ response | exit failure | timeout    [timer effect]
//! ```
//!
//! Laws:
//! - Target binding: `begin` admits a request only when `parse(request.target) = route.target`.
//! - Ownership: a circuit id is pending iff its [`PendingOnionHttpsRequest`] guard is alive and no
//!   terminal outcome has been delivered. Dropping the guard (caller cancellation, timeout, send
//!   failure) removes the entry, so the table never outlives its waiters.
//! - Authentication: a pending request resolves only from its expected return peer, and only with a
//!   payload verified under its return id, selected exit and overlay network. A payload from any
//!   other peer leaves the entry pending, so a misrouted frame cannot cancel the request.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::oneshot;
use futures::TryFutureExt;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_runtime::sleep;
use serde::Deserialize;
use serde::Serialize;

use super::decode_https_payload;
use super::default_method;
use super::default_path;
use super::encode_https_payload;
use super::normalize_method;
use super::normalize_path;
use super::pending::PendingOnionHttpsRequest;
use super::OnionHttpsPayload;
use super::OnionHttpsRequest;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::encode_initial_forward_link;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitPayload;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::circuit::OnionLink;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::circuit::OnionReturnId;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionRouteError;
use crate::sync_lock::lock;

/// Longest wait for the exit's response once the forward cell has reached the first hop.
const ONION_HTTPS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Caller-facing request fields for one HTTPS proxy request.
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

/// Caller-facing response fields returned from one HTTPS proxy request.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct OnionHttpsClientResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Terminal outcome delivered to the waiter of one pending circuit.
pub(super) type OnionHttpsOutcome = Result<OnionHttpsClientResponse>;

/// Verification context and waiter of one pending circuit.
struct PendingRequest {
    expected_return_peer: Did,
    expected_exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    sender: oneshot::Sender<OnionHttpsOutcome>,
}

/// Pending-request table and forward-link capability of one node's HTTPS onion client.
pub(crate) struct OnionHttpsClient {
    pending: Mutex<HashMap<OnionCircuitId, PendingRequest>>,
    /// Delegatee key the exit addresses backward payloads to.
    return_key: PublicKey<33>,
    link_sender: OnionLinkSender,
}

/// One sealed request whose circuit is already pending.
///
/// `first_link` and `cell` are the send effect's data; `response` owns the circuit, so dropping the
/// flight before or after the send cancels it.
pub(crate) struct OnionHttpsFlight {
    first_link: OnionLink,
    cell: Bytes,
    response: PendingOnionHttpsRequest,
}

impl OnionHttpsFlight {
    /// Surrender the send data and keep only the circuit's response guard.
    #[cfg(test)]
    pub(crate) fn into_response(self) -> PendingOnionHttpsRequest {
        self.response
    }
}

impl OnionHttpsClient {
    /// Create an empty client whose backward payloads are addressed to `return_key`.
    pub(crate) fn new(return_key: PublicKey<33>, link_sender: OnionLinkSender) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            return_key,
            link_sender,
        }
    }

    /// Send one request over `route` and wait for its authenticated response.
    ///
    /// Dropping the returned future cancels the pending circuit; an exit that stays silent for
    /// [`ONION_HTTPS_RESPONSE_TIMEOUT`] yields [`Error::OnionProxyRequestTimedOut`].
    pub(crate) async fn request(
        self: &Arc<Self>,
        scope: Scope,
        route: &OnionProxyRoute,
        request: OnionHttpsRequest,
    ) -> OnionHttpsOutcome {
        let OnionHttpsFlight {
            first_link,
            cell,
            response,
        } = self.begin(route, request)?;
        self.link_sender
            .send_sealed(scope, first_link, cell)
            .await?;
        response
            .within(sleep(ONION_HTTPS_RESPONSE_TIMEOUT).map_err(Error::Timer))
            .await
    }

    /// Bind `request` to `route`, register its circuit and seal the first forward cell.
    ///
    /// Post: on `Ok`, exactly one new circuit is pending and owned by the returned flight; on
    /// `Err`, the pending table is unchanged.
    pub(crate) fn begin(
        self: &Arc<Self>,
        route: &OnionProxyRoute,
        request: OnionHttpsRequest,
    ) -> Result<OnionHttpsFlight> {
        if OnionProxyTarget::parse_authority(request.target.as_str())? != route.target {
            return Err(Error::OnionRouteError(
                OnionRouteError::HttpsTargetMismatch {
                    request_target: request.target,
                    route_target: route.target.authority(),
                },
            ));
        }
        let payload = encode_https_payload(OnionHttpsPayload::Request(request))?;
        let client_return = OnionClientReturn::new(self.return_key);
        let (circuit_id, response) = self.register(
            route_first_hop(&route.route)?,
            route.route.exit().clone(),
            client_return.return_id,
        )?;
        // An encoding failure drops `response`, which unregisters the circuit.
        let (first_link, cell) =
            encode_initial_forward_link(client_return, &route.route, circuit_id, payload)?;
        Ok(OnionHttpsFlight {
            first_link,
            cell,
            response,
        })
    }

    /// Allocate a fresh circuit id expected to complete from `expected_return_peer`.
    fn register(
        self: &Arc<Self>,
        expected_return_peer: Did,
        expected_exit: OnionExitDescriptor,
        return_id: OnionReturnId,
    ) -> Result<(OnionCircuitId, PendingOnionHttpsRequest)> {
        let mut pending = lock(&self.pending)?;
        for _ in 0..16 {
            let id = OnionCircuitId::random();
            if let Entry::Vacant(entry) = pending.entry(id) {
                let (sender, receiver) = oneshot::channel();
                entry.insert(PendingRequest {
                    expected_return_peer,
                    expected_exit,
                    return_id,
                    sender,
                });
                return Ok((
                    id,
                    PendingOnionHttpsRequest::new(Arc::clone(self), id, receiver),
                ));
            }
        }
        Err(Error::OnionRouteError(
            OnionRouteError::CircuitIdAllocationFailed,
        ))
    }

    /// Remove one circuit whose waiter no longer exists.
    pub(super) fn cancel(&self, id: OnionCircuitId) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// Deliver one authenticated backward payload to the pending request owning `circuit_id`.
    ///
    /// Returns the payload unchanged when no pending HTTPS request owns the circuit, so another
    /// client adapter installed on the same circuit protocol may claim it. A payload from a peer
    /// other than the expected return peer is claimed and discarded with the entry left pending.
    pub(crate) fn complete_payload(
        &self,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
        network_id: u32,
    ) -> Result<Option<OnionAuthenticatedPayload>> {
        let request = {
            let mut pending = lock(&self.pending)?;
            let Entry::Occupied(entry) = pending.entry(circuit_id) else {
                return Ok(Some(payload));
            };
            if entry.get().expected_return_peer != from {
                return Ok(None);
            }
            entry.remove()
        };
        let outcome = payload
            .into_verified_payload(request.return_id, &request.expected_exit, network_id)
            .and_then(|verified| client_outcome(verified.payload));
        // A closed receiver means the waiter was dropped after removal; nobody observes the outcome.
        let _ = request.sender.send(outcome);
        Ok(None)
    }

    /// Number of circuits awaiting a terminal outcome.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }

    /// Return id a pending circuit expects its exit to sign.
    #[cfg(test)]
    pub(crate) fn pending_return_id(&self, id: OnionCircuitId) -> Option<OnionReturnId> {
        self.pending
            .lock()
            .ok()
            .and_then(|pending| pending.get(&id).map(|request| request.return_id))
    }
}

/// Interpret one verified backward payload as the client's terminal outcome.
fn client_outcome(payload: OnionCircuitPayload) -> OnionHttpsOutcome {
    match decode_https_payload(payload)? {
        Some(OnionHttpsPayload::Response(response)) => Ok(OnionHttpsClientResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        }),
        Some(OnionHttpsPayload::Error(failure)) => Err(Error::OnionRouteError(
            OnionRouteError::ExitFailure(failure),
        )),
        Some(OnionHttpsPayload::Request(_)) | None => Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedBackwardPayload,
        )),
    }
}

/// Parse a full HTTPS URL and encode one client request for its target.
pub fn client_request_from_url(
    url: &str,
    request: OnionHttpsClientRequest,
) -> Result<(OnionProxyTarget, OnionHttpsRequest)> {
    let (target, path) = parse_https_url(url)?;
    let request = client_request_with_default_path(&target, request, path.as_str())?;
    Ok((target, request))
}

/// Encode `request` for `target`, using `default_path` when the caller gave no path override.
pub(super) fn client_request_with_default_path(
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

/// Split an absolute `https://` URL into its canonical target and its path-and-query.
fn parse_https_url(url: &str) -> Result<(OnionProxyTarget, String)> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        Error::HttpRequestError("HTTPS onion proxy request URL must be absolute".to_string())
    })?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(Error::HttpRequestError(format!(
            "HTTPS onion proxy only supports https URLs, got scheme {scheme:?}"
        )));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, suffix) = rest.split_at(authority_end);
    if authority.contains('@') {
        return Err(Error::HttpRequestError(
            "HTTPS onion proxy URLs must not contain userinfo".to_string(),
        ));
    }
    let authority = https_authority_with_default_port(authority)?;
    let target = OnionProxyTarget::parse_authority(authority.as_str())?;
    Ok((target, url_path(suffix)))
}

/// Validate a URL authority and make its port explicit, defaulting to 443.
fn https_authority_with_default_port(authority: &str) -> Result<String> {
    let authority = authority.trim();
    if authority.is_empty() {
        return Err(Error::HttpRequestError(
            "HTTPS onion proxy URL host must not be empty".to_string(),
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
                "HTTPS onion proxy URL host must not be empty".to_string(),
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

/// Path-and-query of the URL suffix after the authority, with the fragment removed.
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
