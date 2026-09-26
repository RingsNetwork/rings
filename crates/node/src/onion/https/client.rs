//! Client half of the HTTPS onion protocol, shared by native and browser callers.
//!
//! One request is one session. Route selection is the caller's platform decision (a browser must
//! start at a direct peer and may read a remote directory); everything after it is target
//! independent:
//!
//! ```text
//! url ──OnionHttpsCall::from_url──▶ (target, call)
//!     ──route(target)───────────▶ OnionProxyRoute                      [caller]
//!     ──open(route, https, t)───▶ session                              [exit answers the open]
//!     ──data(enc(request)) · fin─▶ h                                    [the exit fetches]
//!     ◀──data(enc(outcome)) · fin─ h ──▶ response | exit failure       [within the deadline]
//! ```
//!
//! Laws:
//! - Target binding: an [`OnionHttpsCall`] names no target; the session is opened for
//!   `route.target`, whose digest `h` binds, so the request cannot disagree with its route.
//! - Ownership: the session lives as long as the request's future; dropping it (caller
//!   cancellation, the deadline) ends the session's driver and releases its tags.
//! - Authentication: the response arrives in replies that open under the session's reply keys,
//!   which only the client and the exit's process hold (#834 Prop. Reply authentication).

use std::time::Duration;

use bytes::Bytes;
use futures::future::Either;
use futures::FutureExt;
use rings_runtime::sleep;
use serde::Deserialize;

use super::decode_outcome;
use super::default_method;
use super::default_path;
use super::encode_request;
use super::normalize_method;
use super::normalize_path;
use super::OnionHttpsOutcome;
use super::OnionHttpsRequest;
use super::OnionHttpsResponse;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::runtime::OnionRuntime;
use crate::onion::session::client::OnionCreditWindow;
use crate::onion::session::dial::OnionSessionRequest;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;

/// Longest wait for the exit's whole response once the session has opened.
const ONION_HTTPS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest encoded response the client collects: the exit's body limit and its framing.
const MAX_HTTPS_OUTCOME_BYTES: usize = 9 * 1024 * 1024;

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

    /// The wire request this call becomes; its authority is the session's target.
    pub(super) fn into_request(self) -> OnionHttpsRequest {
        OnionHttpsRequest {
            method: self.method,
            path: self.path,
            headers: self.headers,
            body: self.body,
        }
    }
}

/// The HTTPS onion client of one node: every request is a session over the node's runtime.
#[derive(Clone)]
pub(crate) struct OnionHttpsClient {
    /// The node's onion runtime.
    runtime: OnionRuntime,
}

impl OnionHttpsClient {
    /// The client over `runtime`.
    pub(crate) const fn new(runtime: OnionRuntime) -> Self {
        Self { runtime }
    }

    /// Send `call` to `route.target` over `route` and wait for the exit's response.
    ///
    /// Dropping the returned future ends the session; an exit that does not answer within
    /// [`ONION_HTTPS_RESPONSE_TIMEOUT`] of the open yields [`Error::OnionProxyRequestTimedOut`].
    ///
    /// # Errors
    ///
    /// The session's refusal or failure, the exit's reported failure, or the timeout.
    pub(crate) async fn request(
        &self,
        route: &OnionProxyRoute,
        call: OnionHttpsCall,
    ) -> Result<OnionHttpsResponse> {
        let request = encode_request(&call.into_request())?;
        let (mut sender, mut receiver) = self
            .runtime
            .open(OnionSessionRequest {
                route: route.route.clone(),
                symbol: OnionServiceName::https(),
                target: route.target.clone(),
                class: OnionLoopClass::DEFAULT,
                window: OnionCreditWindow::DEFAULT,
            })
            .await?
            .split();
        sender.send(Bytes::from(request)).await?;
        sender.fin().await?;
        let collect = async {
            let mut outcome = Vec::new();
            loop {
                match receiver.next().await {
                    Some(OnionStreamEvent::Data(bytes)) => {
                        if outcome.len().saturating_add(bytes.len()) > MAX_HTTPS_OUTCOME_BYTES {
                            return Err(Error::NoPermission);
                        }
                        outcome.extend_from_slice(&bytes);
                    }
                    Some(OnionStreamEvent::Fin) => return client_outcome(&outcome),
                    Some(OnionStreamEvent::Failed) | None => {
                        return Err(Error::OnionRouteError(OnionRouteError::HttpsResponseClosed))
                    }
                }
            }
        };
        let deadline = sleep(ONION_HTTPS_RESPONSE_TIMEOUT).fuse();
        futures::pin_mut!(collect, deadline);
        match futures::future::select(collect, deadline).await {
            Either::Left((outcome, _)) => outcome,
            Either::Right(_) => Err(Error::OnionProxyRequestTimedOut),
        }
    }
}

/// Interpret the world-to-client stream of a fetch as the client's result.
fn client_outcome(bytes: &[u8]) -> Result<OnionHttpsResponse> {
    match decode_outcome(bytes)? {
        OnionHttpsOutcome::Response(response) => Ok(response),
        OnionHttpsOutcome::Error(failure) => Err(Error::OnionRouteError(
            OnionRouteError::ExitFailure(failure),
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
