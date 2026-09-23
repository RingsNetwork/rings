//! Client half of the HTTPS onion protocol, shared by native and browser callers.
//!
//! One request owns one circuit. Route selection is the caller's platform decision (a browser must
//! start at a direct peer and may read a remote directory); everything after it is target
//! independent:
//!
//! ```text
//! url ──OnionHttpsCall::from_url──▶ (target, call)
//!     ──route(target)───────────▶ OnionProxyRoute                      [caller]
//!     ──begin(route, call)──────▶ OnionHttpsFlight {link, cell, guard} [pending table]
//!     ──send_sealed─────────────▶ first hop                            [link effect]
//!     ──within(deadline)────────▶ response | exit failure | timeout    [timer effect]
//! ```
//!
//! Laws:
//! - Target binding: an [`OnionHttpsCall`] names no target; `begin` addresses it to
//!   `route.target`, so the request target equals the route target by construction.
//! - Ownership: a circuit id is pending iff its [`PendingOnionHttpsRequest`] guard is alive and no
//!   terminal outcome has been delivered. Dropping the guard (caller cancellation, timeout, send
//!   failure) removes the entry, so the table never outlives its waiters.
//! - Authentication: a pending request owns the pair `(circuit id, expected return peer)`.
//!   [`OnionHttpsClient::claim`] yields it only for that pair, and [`OnionHttpsClaim::resolve`]
//!   delivers a payload only after verifying it under the request's return id, selected exit and
//!   overlay network. Any other pair leaves the table unchanged, so a misrouted frame can neither
//!   resolve nor cancel a request.

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

use super::decode_https_payload;
use super::default_method;
use super::default_path;
use super::encode_https_payload;
use super::normalize_method;
use super::normalize_path;
use super::pending::PendingOnionHttpsRequest;
use super::OnionHttpsPayload;
use super::OnionHttpsRequest;
use super::OnionHttpsResponse;
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

/// Longest wait for the exit's response once the first hop has accepted the forward cell.
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

/// Normalized HTTPS request that names no target.
///
/// Constructing a call is the proof that its method and path are normalized. A call is addressed
/// only when it is sent, to the target of the route it is sent over, so a request cannot disagree
/// with its route.
#[derive(Debug)]
pub struct OnionHttpsCall {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl OnionHttpsCall {
    /// Split an absolute `https://` URL into the target to route to and the normalized call.
    ///
    /// The URL path is the default; `request.path` overrides it.
    pub fn from_url(
        url: &str,
        request: OnionHttpsClientRequest,
    ) -> Result<(OnionProxyTarget, Self)> {
        let (target, path) = parse_https_url(url)?;
        Ok((target, Self::with_default_path(request, path.as_str())?))
    }

    /// Normalize `request`, using `default_path` when it carries no path override.
    pub(super) fn with_default_path(
        request: OnionHttpsClientRequest,
        default_path: &str,
    ) -> Result<Self> {
        Ok(Self {
            method: normalize_method(&request.method),
            path: normalize_path(request.path.as_deref().unwrap_or(default_path))?,
            headers: request.headers,
            body: request.body,
        })
    }

    /// The wire request this call becomes when sent to `target`.
    pub(super) fn addressed_to(self, target: &OnionProxyTarget) -> OnionHttpsRequest {
        OnionHttpsRequest {
            target: target.authority(),
            method: self.method,
            path: self.path,
            headers: self.headers,
            body: self.body,
        }
    }
}

/// Terminal outcome delivered to the waiter of one pending circuit.
pub(super) type OnionHttpsOutcome = Result<OnionHttpsResponse>;

/// Verification context and waiter of one pending circuit.
struct PendingRequest {
    expected_return_peer: Did,
    expected_exit: OnionExitDescriptor,
    return_id: OnionReturnId,
    sender: oneshot::Sender<OnionHttpsOutcome>,
}

impl PendingRequest {
    /// Whether `peer` is the immediate return peer this request's route ends its backward path at.
    fn answers_through(&self, peer: Did) -> bool {
        self.expected_return_peer == peer
    }
}

/// Exclusive right to resolve one pending request, taken out of the table by
/// [`OnionHttpsClient::claim`].
///
/// Linear by intent: [`resolve`](Self::resolve) consumes it; dropping it unresolved closes the
/// waiter with [`OnionRouteError::HttpsResponseClosed`].
pub(crate) struct OnionHttpsClaim(PendingRequest);

impl OnionHttpsClaim {
    /// Verify `payload` for the claimed request and hand its terminal outcome to the waiter.
    ///
    /// A verification or decoding failure is itself the outcome; nothing is returned to the caller.
    pub(crate) fn resolve(self, payload: OnionAuthenticatedPayload, network_id: u32) {
        let Self(request) = self;
        let outcome = payload
            .into_verified_payload(request.return_id, &request.expected_exit, network_id)
            .and_then(|verified| client_outcome(verified.payload));
        // A closed receiver means the waiter was dropped after the claim; nobody observes it.
        let _ = request.sender.send(outcome);
    }
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

    /// Send `call` over `route` and wait for its authenticated response.
    ///
    /// The deadline, [`ONION_HTTPS_RESPONSE_TIMEOUT`], starts once the first hop has accepted the
    /// forward cell; the send itself is bounded by the link outbox, not by this deadline. Dropping
    /// the returned future cancels the pending circuit; a silent exit yields
    /// [`Error::OnionProxyRequestTimedOut`].
    pub(crate) async fn request(
        self: &Arc<Self>,
        scope: Scope,
        route: &OnionProxyRoute,
        call: OnionHttpsCall,
    ) -> OnionHttpsOutcome {
        let OnionHttpsFlight {
            first_link,
            cell,
            response,
        } = self.begin(route, call)?;
        self.link_sender
            .send_sealed(scope, first_link, cell)
            .await?;
        response
            .within(sleep(ONION_HTTPS_RESPONSE_TIMEOUT).map_err(Error::Timer))
            .await
    }

    /// Address `call` to `route.target`, register its circuit and seal the first forward cell.
    ///
    /// Post: on `Ok`, exactly one new circuit is pending and owned by the returned flight; on
    /// `Err`, the pending table is unchanged.
    pub(crate) fn begin(
        self: &Arc<Self>,
        route: &OnionProxyRoute,
        call: OnionHttpsCall,
    ) -> Result<OnionHttpsFlight> {
        let payload =
            encode_https_payload(OnionHttpsPayload::Request(call.addressed_to(&route.target)))?;
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

    /// Take the pending request owning `(circuit_id, from)` out of the table.
    ///
    /// Post: `Some` iff such a request existed, and it is no longer pending; `None` leaves the table
    /// unchanged, so the backward payload belongs to another adapter or to nobody.
    pub(crate) fn claim(
        &self,
        from: Did,
        circuit_id: OnionCircuitId,
    ) -> Result<Option<OnionHttpsClaim>> {
        let mut pending = lock(&self.pending)?;
        Ok(match pending.entry(circuit_id) {
            Entry::Occupied(entry) if entry.get().answers_through(from) => {
                Some(OnionHttpsClaim(entry.remove()))
            }
            Entry::Occupied(_) | Entry::Vacant(_) => None,
        })
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
        Some(OnionHttpsPayload::Response(response)) => Ok(response),
        Some(OnionHttpsPayload::Error(failure)) => Err(Error::OnionRouteError(
            OnionRouteError::ExitFailure(failure),
        )),
        Some(OnionHttpsPayload::Request(_)) | None => Err(Error::OnionRouteError(
            OnionRouteError::UnexpectedBackwardPayload,
        )),
    }
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
