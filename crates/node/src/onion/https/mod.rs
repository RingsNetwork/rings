//! `https`: one HTTPS request/response fetch over an onion session (#834 D1′, D2′).
//!
//! `https` is a fetch only; every byte tunnel, TLS included, is `tcp`. A fetch is a session whose
//! client-to-world stream is one encoded request and whose world-to-client stream is one encoded
//! outcome:
//!
//! ```text
//! client ── data(…enc(OnionHttpsRequest)…) ── fin ──▶ h ── fetch(t, request) ──▶ world
//! client ◀── data(…enc(OnionHttpsOutcome)…) ── fin ── h
//! ```
//!
//! The session's target `t`, bound at `h` by its digest, is the fetch's authority, so the request
//! names none and cannot disagree with it. The exit half is the fetch world
//! (`OnionHttpsWorld`), interpreted by the session shell; the client half is in `client`.
//!
//! A browser page exit is constrained by the host browser's `fetch` capability: CORS, forbidden
//! headers, credentials policy, and extension host permissions still apply. A full arbitrary HTTPS
//! exit must run in a browser-extension or native context that grants those fetch permissions.

use bytes::Bytes;
use futures::channel::oneshot;
use rings_core::utils::get_epoch_ms;
use serde::Deserialize;
use serde::Serialize;

#[cfg(rings_browser)]
use self::browser::execute_https_request;
pub use self::client::OnionHttpsCall;
pub(crate) use self::client::OnionHttpsClient;
pub use self::client::OnionHttpsClientRequest;
#[cfg(all(test, rings_native))]
use self::limits::checked_status_code;
use self::limits::https_response_body_limit;
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
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::session::serve::OnionWorld;
use crate::onion::session::serve::OnionWorldReader;
use crate::onion::session::serve::OnionWorldWriter;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;

/// The default and largest response body an exit fetches for one request.
const DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

/// The largest encoded request an exit buffers for one session.
const MAX_HTTPS_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// One HTTPS request of a fetch session; its authority is the session's target.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionHttpsRequest {
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

/// What a fetch session's world-to-client stream encodes: the response, or why there is none.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum OnionHttpsOutcome {
    /// The exit fetched a response.
    Response(OnionHttpsResponse),
    /// The exit could not fetch one.
    Error(OnionExitFailure),
}

/// Encode a request, the client-to-world stream of a fetch.
fn encode_request(request: &OnionHttpsRequest) -> Result<Vec<u8>> {
    rings_codec::serialize(request).map_err(|_| Error::EncodeError)
}

/// Encode an outcome, the world-to-client stream of a fetch.
fn encode_outcome(outcome: &OnionHttpsOutcome) -> Result<Vec<u8>> {
    rings_codec::serialize(outcome).map_err(|_| Error::EncodeError)
}

/// Decode the world-to-client stream of a fetch.
fn decode_outcome(bytes: &[u8]) -> Result<OnionHttpsOutcome> {
    rings_codec::deserialize(bytes).map_err(|_| Error::DecodeError)
}

/// Where a fetch world's requests leave: the platform's fetch, or a test's function of the
/// target and the request.
#[derive(Clone, Copy)]
pub(crate) enum OnionHttpsEgress {
    /// The platform's fetch (`native` or `browser`).
    Platform,
    /// A test egress answering every request in place of the network.
    #[cfg(all(test, rings_native))]
    Test(fn(&OnionProxyTarget, &OnionHttpsRequest) -> OnionHttpsResponse),
}

/// The fetch world of an `https` exit: a session's request, fetched once its stream ends.
pub(crate) struct OnionHttpsWorld {
    /// The exit policy targets are admitted under.
    policy: OnionExitPolicy,
    /// The node-wide exit accounting, which bounds the response bytes.
    accounting: OnionExitAccounting,
    /// Where the requests leave.
    egress: OnionHttpsEgress,
}

impl OnionHttpsWorld {
    /// The fetch world of an exit under `policy` and `accounting`, fetching through the platform.
    pub(crate) const fn new(policy: OnionExitPolicy, accounting: OnionExitAccounting) -> Self {
        Self::with_egress(policy, accounting, OnionHttpsEgress::Platform)
    }

    /// The fetch world of an exit whose requests leave through `egress`.
    pub(crate) const fn with_egress(
        policy: OnionExitPolicy,
        accounting: OnionExitAccounting,
        egress: OnionHttpsEgress,
    ) -> Self {
        Self {
            policy,
            accounting,
            egress,
        }
    }
}

/// The write half of a fetch: it buffers the encoded request and hands it over at `fin`.
pub(crate) struct OnionHttpsWriter {
    /// The request bytes so far.
    request: Vec<u8>,
    /// Where the complete request goes; spent at `fin`.
    complete: Option<oneshot::Sender<Vec<u8>>>,
}

/// The read half of a fetch: it waits for the request, fetches, and serves the encoded outcome.
pub(crate) struct OnionHttpsReader {
    /// The session's target, the fetch's authority.
    target: OnionProxyTarget,
    /// The policy, for the fetch's byte accounting.
    policy: OnionExitPolicy,
    /// The accounting.
    accounting: OnionExitAccounting,
    /// Where the request leaves.
    egress: OnionHttpsEgress,
    /// The complete request, until it arrives.
    request: Option<oneshot::Receiver<Vec<u8>>>,
    /// The encoded outcome not yet read.
    outcome: Bytes,
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl OnionWorld for OnionHttpsWorld {
    type Reader = OnionHttpsReader;
    type Writer = OnionHttpsWriter;

    /// The fetch records the response's headers and body as they stream.
    const RECORDS_OWN_READS: bool = true;

    async fn open(&self, target: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)> {
        let (complete, request) = oneshot::channel();
        Ok((
            OnionHttpsReader {
                target: target.clone(),
                policy: self.policy.clone(),
                accounting: self.accounting.clone(),
                egress: self.egress,
                request: Some(request),
                outcome: Bytes::new(),
            },
            OnionHttpsWriter {
                request: Vec::new(),
                complete: Some(complete),
            },
        ))
    }
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl OnionWorldWriter for OnionHttpsWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<()> {
        if self.request.len().saturating_add(bytes.len()) > MAX_HTTPS_REQUEST_BYTES {
            return Err(Error::NoPermission);
        }
        self.request.extend_from_slice(&bytes);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(complete) = self.complete.take() {
            let _ = complete.send(std::mem::take(&mut self.request));
        }
        Ok(())
    }
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl OnionWorldReader for OnionHttpsReader {
    /// The first read waits for the whole request, fetches it and encodes the outcome; every read
    /// then serves the next `max` bytes of it, and `None` after the last.
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>> {
        if let Some(request) = self.request.take() {
            // A session that closes before its `fin` has no request, and ends here.
            let Ok(request) = request.await else {
                return Ok(None);
            };
            let outcome = match rings_codec::deserialize::<OnionHttpsRequest>(&request) {
                Ok(request) => match self.fetch(&request).await {
                    Ok(response) => OnionHttpsOutcome::Response(response),
                    Err(error) => OnionHttpsOutcome::Error(OnionExitFailure::from_error(&error)),
                },
                Err(_) => OnionHttpsOutcome::Error(OnionExitFailure::MalformedRequest),
            };
            self.outcome = Bytes::from(encode_outcome(&outcome)?);
        }
        if self.outcome.is_empty() {
            return Ok(None);
        }
        let length = max.min(self.outcome.len());
        Ok(Some(self.outcome.split_to(length)))
    }
}

impl OnionHttpsReader {
    /// Fetch `request` at the session's target through the reader's egress.
    async fn fetch(&self, request: &OnionHttpsRequest) -> Result<OnionHttpsResponse> {
        match self.egress {
            OnionHttpsEgress::Platform => {
                fetch(&self.target, request, &self.policy, &self.accounting).await
            }
            #[cfg(all(test, rings_native))]
            OnionHttpsEgress::Test(respond) => Ok(respond(&self.target, request)),
        }
    }
}

/// Fetch `request` at `target`, the session's authority, recording the response's headers and
/// body chunks against the policy's byte budget as they arrive, and refusing past it: the
/// budget holds across every concurrent fetch, and the session shell records none of these bytes
/// again ([`OnionWorld::RECORDS_OWN_READS`]).
async fn fetch(
    target: &OnionProxyTarget,
    request: &OnionHttpsRequest,
    policy: &OnionExitPolicy,
    accounting: &OnionExitAccounting,
) -> Result<OnionHttpsResponse> {
    let body_limit = https_response_body_limit(accounting.remaining_bytes(policy, get_epoch_ms())?);
    if body_limit == 0 {
        return Err(Error::NoPermission);
    }
    let url = format!(
        "https://{}{}",
        target.authority(),
        normalize_path(&request.path)?
    );
    let response = execute_https_request(&url, target, request, body_limit, |bytes| {
        accounting.record_bytes(policy, bytes, get_epoch_ms())
    })
    .await?;
    Ok(OnionHttpsResponse {
        status: response.status,
        headers: response.headers,
        body: response.body,
    })
}

/// A response as a platform's fetch returns it.
pub(super) struct FetchResponse {
    /// HTTP status code.
    status: u16,
    /// Response headers.
    headers: Vec<(String, String)>,
    /// Response body bytes.
    body: Vec<u8>,
}

/// The canonical form of a method: trimmed, upper case, `GET` when empty.
fn normalize_method(method: &str) -> String {
    let method = method.trim();
    if method.is_empty() {
        default_method()
    } else {
        method.to_ascii_uppercase()
    }
}

/// The canonical form of a path and query: `/` when empty, `/?q` for a bare query.
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

/// The default method, `GET`.
fn default_method() -> String {
    "GET".to_string()
}

/// The default path, `/`.
fn default_path() -> String {
    "/".to_string()
}

#[cfg(all(test, rings_native))]
mod tests;

#[cfg(rings_browser)]
mod browser;
mod client;
mod limits;
#[cfg(rings_native)]
mod native;
