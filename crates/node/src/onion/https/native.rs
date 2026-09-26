use std::net::SocketAddr;
use std::time::Duration;

use super::limits::headers_bytes;
use super::limits::reject_content_length_over_limit;
use super::limits::usize_to_u64;
use super::normalize_method;
use super::FetchResponse;
use super::OnionHttpsRequest;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::target::resolve_target_addresses;
use crate::onion::target::select_public_exit_addresses;
use crate::onion::target::PublicAddressSelection;

const HTTPS_EXIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Return whether the native exit, rather than the onion client, owns this request header.
///
/// The validated URL is the sole authority source. Message framing and hop-by-hop behavior also
/// belong to reqwest so untrusted callers cannot override transport semantics.
pub(super) fn is_native_transport_managed_header(name: &str) -> bool {
    [
        "host",
        ":authority",
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "content-length",
        "upgrade",
        "expect",
    ]
    .iter()
    .any(|managed| name.eq_ignore_ascii_case(managed))
}

/// Native HTTPS egress: the target host pinned to its resolved public addresses.
///
/// A request leaves only after resolution selected public addresses; pinning them, with every
/// ambient proxy disabled, keeps reqwest from re-resolving the host or routing around the check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NativeHttpsEgress {
    /// The host name the request is addressed to.
    pub(super) host: String,
    /// The public addresses `host` is pinned to.
    pub(super) addresses: Vec<SocketAddr>,
}

impl NativeHttpsEgress {
    /// Pin `host` to `addresses` on `builder`, disabling every ambient proxy.
    fn configure(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder
            .no_proxy()
            .resolve_to_addrs(&self.host, self.addresses.as_slice())
    }
}

pub(super) async fn execute_https_request(
    url: &str,
    target: &OnionProxyTarget,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
    record_bytes: impl Fn(u64) -> Result<()>,
) -> Result<FetchResponse> {
    let addresses = resolve_target_addresses(target).await?;
    let egress = select_native_https_egress(target, addresses)?;
    native_fetch_with_timeout(
        url,
        request,
        max_body_bytes,
        HTTPS_EXIT_REQUEST_TIMEOUT,
        &egress,
        record_bytes,
    )
    .await
}

/// Select native HTTPS egress from an immutable resolution result.
///
/// Post: public resolution selects the pinned path; every non-public result is denied.
pub(super) fn select_native_https_egress(
    target: &OnionProxyTarget,
    addresses: Vec<SocketAddr>,
) -> Result<NativeHttpsEgress> {
    match select_public_exit_addresses(addresses) {
        PublicAddressSelection::Public(addresses) => Ok(NativeHttpsEgress {
            host: target.host().to_string(),
            addresses,
        }),
        PublicAddressSelection::Denied => Err(Error::NoPermission),
        PublicAddressSelection::Empty => Err(Error::OnionTargetResolvedEmpty {
            authority: target.authority(),
        }),
    }
}

fn native_http_error(context: &str, error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::HttpRequestError(format!("{context}: timed out"))
    } else {
        Error::HttpRequestError(format!("{context}: {error}"))
    }
}

pub(super) async fn native_fetch_with_timeout(
    url: &str,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
    timeout: Duration,
    egress: &NativeHttpsEgress,
    record_bytes: impl Fn(u64) -> Result<()>,
) -> Result<FetchResponse> {
    let method = reqwest::Method::from_bytes(normalize_method(&request.method).as_bytes())
        .map_err(|error| Error::HttpRequestError(format!("invalid HTTPS proxy method: {error}")))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    let client = egress
        .configure(client)
        .build()
        .map_err(|error| Error::HttpRequestError(format!("build HTTPS proxy client: {error}")))?;
    let mut builder = client.request(method, url);
    for (name, value) in &request.headers {
        if is_native_transport_managed_header(name) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }
    let mut response = builder
        .send()
        .await
        .map_err(|error| native_http_error("native HTTPS proxy request", error))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();
    reject_content_length_over_limit(&headers, max_body_bytes)?;
    record_bytes(headers_bytes(&headers)?)?;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| native_http_error("read HTTPS proxy response", error))?
    {
        let body_len = usize_to_u64(body.len())?;
        let chunk_len = usize_to_u64(chunk.len())?;
        if max_body_bytes > 0 && body_len.saturating_add(chunk_len) > max_body_bytes {
            return Err(Error::NoPermission);
        }
        record_bytes(chunk_len)?;
        body.extend_from_slice(chunk.as_ref());
    }
    Ok(FetchResponse {
        status,
        headers,
        body,
    })
}
